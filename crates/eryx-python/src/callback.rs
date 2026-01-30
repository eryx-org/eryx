//! Python callback support for Eryx.
//!
//! This module provides the ability to register Python functions as callbacks
//! that can be invoked from sandboxed Python code.
//!
//! # Example
//!
//! ```python
//! import eryx
//!
//! # Option A: Dict-based API
//! def get_time():
//!     import time
//!     return {"timestamp": time.time()}
//!
//! sandbox = eryx.Sandbox(
//!     callbacks=[
//!         {"name": "get_time", "fn": get_time, "description": "Returns current timestamp"},
//!     ]
//! )
//!
//! # Option B: Decorator-based API
//! registry = eryx.CallbackRegistry()
//!
//! @registry.callback(description="Returns current timestamp")
//! def get_time():
//!     import time
//!     return {"timestamp": time.time()}
//!
//! sandbox = eryx.Sandbox(callbacks=registry)
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use eryx::{Callback, CallbackError, Schema};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use serde_json::Value;

/// A Python callable wrapped as a Rust `Callback`.
///
/// This struct implements the `eryx::Callback` trait, allowing Python functions
/// to be invoked from sandboxed code. Supports both sync and async Python functions.
pub struct PythonCallback {
    name: String,
    description: String,
    callable: Py<PyAny>,
    schema: Schema,
    /// Whether this is an async Python function (coroutine function).
    is_async: bool,
}

impl std::fmt::Debug for PythonCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonCallback")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("schema", &self.schema)
            .field("is_async", &self.is_async)
            .field("callable", &"<Python callable>")
            .finish()
    }
}

impl PythonCallback {
    /// Create a new `PythonCallback` from components.
    pub fn new(
        name: String,
        description: String,
        callable: Py<PyAny>,
        schema: Schema,
        is_async: bool,
    ) -> Self {
        Self {
            name,
            description,
            callable,
            schema,
            is_async,
        }
    }
}

// SAFETY: `Py<PyAny>` is Send + Sync as long as we only access it with the GIL held.
// We always use `Python::with_gil()` when accessing the callable.
unsafe impl Send for PythonCallback {}
unsafe impl Sync for PythonCallback {}

impl Callback for PythonCallback {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Schema {
        self.schema.clone()
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        // Clone the Py<PyAny> by acquiring the GIL briefly
        let callable = Python::with_gil(|py| self.callable.clone_ref(py));
        let is_async = self.is_async;

        Box::pin(async move {
            // Use spawn_blocking to avoid blocking the tokio runtime while holding the GIL
            tokio::task::spawn_blocking(move || {
                Python::with_gil(|py| {
                    // Convert JSON args to Python kwargs dict
                    let kwargs = json_to_py_kwargs(py, &args)?;

                    // Call the Python function with kwargs
                    let result = callable
                        .call(py, (), Some(&kwargs))
                        .map_err(|e| format_python_error(py, e))?;

                    // If this is an async function, the result is a coroutine - run it
                    let result = if is_async {
                        run_coroutine(py, result.bind(py))?
                    } else {
                        result
                    };

                    // Convert the result back to JSON
                    pythonize::depythonize(result.bind(py)).map_err(|e| {
                        CallbackError::ExecutionFailed(format!(
                            "Failed to serialize callback result: {e}"
                        ))
                    })
                })
            })
            .await
            .map_err(|e| CallbackError::ExecutionFailed(format!("Callback task failed: {e}")))?
        })
    }
}

/// Run a Python coroutine to completion using asyncio.run().
fn run_coroutine(py: Python<'_>, coro: &Bound<'_, PyAny>) -> Result<PyObject, CallbackError> {
    let asyncio = py
        .import("asyncio")
        .map_err(|e| CallbackError::ExecutionFailed(format!("Failed to import asyncio: {e}")))?;

    asyncio
        .call_method1("run", (coro,))
        .map(|r| r.unbind())
        .map_err(|e| format_python_error(py, e))
}

/// Convert a JSON Value to a Python kwargs dict.
fn json_to_py_kwargs<'py>(
    py: Python<'py>,
    args: &Value,
) -> Result<Bound<'py, PyDict>, CallbackError> {
    let kwargs = PyDict::new(py);

    if let Value::Object(map) = args {
        for (key, value) in map {
            let py_value = pythonize::pythonize(py, value).map_err(|e| {
                CallbackError::InvalidArguments(format!("Failed to convert argument '{key}': {e}"))
            })?;
            kwargs.set_item(key, py_value).map_err(|e| {
                CallbackError::InvalidArguments(format!("Failed to set argument '{key}': {e}"))
            })?;
        }
    } else if !args.is_null() {
        // For non-object, non-null args, this is unexpected
        return Err(CallbackError::InvalidArguments(format!(
            "Expected object or null for callback arguments, got: {args}"
        )));
    }

    Ok(kwargs)
}

/// Format a Python exception with traceback for error messages.
fn format_python_error(py: Python<'_>, err: PyErr) -> CallbackError {
    // Try to get the full traceback
    let traceback = err
        .traceback(py)
        .map(|tb| {
            tb.format()
                .unwrap_or_else(|_| "<traceback unavailable>".to_string())
        })
        .unwrap_or_default();

    let error_msg = if traceback.is_empty() {
        format!("{err}")
    } else {
        format!("{err}\n{traceback}")
    };

    CallbackError::ExecutionFailed(error_msg)
}

/// A registry for collecting callbacks using the decorator pattern.
///
/// Example:
///     registry = eryx.CallbackRegistry()
///
///     @registry.callback(description="Returns current timestamp")
///     def get_time():
///         import time
///         return {"timestamp": time.time()}
///
///     sandbox = eryx.Sandbox(callbacks=registry)
#[pyclass(module = "eryx")]
#[derive(Debug, Default)]
pub struct CallbackRegistry {
    /// Stored callback definitions: (name, description, callable, schema)
    callbacks: Vec<CallbackDef>,
}

/// Internal callback definition stored in the registry.
#[derive(Debug)]
struct CallbackDef {
    name: String,
    description: String,
    callable: Py<PyAny>,
    schema: Option<Value>,
    /// Whether this is an async Python function.
    is_async: bool,
}

impl Clone for CallbackDef {
    fn clone(&self) -> Self {
        Python::with_gil(|py| Self {
            name: self.name.clone(),
            description: self.description.clone(),
            callable: self.callable.clone_ref(py),
            schema: self.schema.clone(),
            is_async: self.is_async,
        })
    }
}

#[pymethods]
impl CallbackRegistry {
    /// Create a new empty callback registry.
    #[new]
    fn new() -> Self {
        Self::default()
    }

    /// Decorator to register a callback function.
    ///
    /// Args:
    ///     name: Optional name for the callback. Defaults to the function's __name__.
    ///     description: Optional description. Defaults to the function's __doc__ or empty string.
    ///     schema: Optional JSON Schema dict for parameters. Auto-inferred if not provided.
    ///
    /// Returns:
    ///     A decorator that registers the function and returns it unchanged.
    ///
    /// Example:
    ///     @registry.callback(description="Echoes the message")
    ///     def echo(message: str, repeat: int = 1):
    ///         return {"echoed": message * repeat}
    #[pyo3(signature = (name=None, description=None, schema=None))]
    fn callback(
        &mut self,
        py: Python<'_>,
        name: Option<String>,
        description: Option<String>,
        schema: Option<Bound<'_, PyAny>>,
    ) -> PyResult<PyObject> {
        // Convert schema from Python to JSON Value if provided
        let schema_value: Option<Value> = schema
            .map(|s| pythonize::depythonize(&s))
            .transpose()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("Invalid schema: {e}")))?;

        // Create a decorator that captures the registration parameters
        let callbacks_ptr = self as *mut CallbackRegistry;

        // We need to create a Python function that acts as a decorator
        // Since we can't easily create closures, we'll use a helper class
        let decorator = DecoratorHelper {
            registry_ptr: callbacks_ptr as usize,
            name,
            description,
            schema: schema_value,
        };

        Ok(decorator.into_pyobject(py)?.into_any().unbind())
    }

    /// Add a callback directly without using the decorator pattern.
    ///
    /// Args:
    ///     fn: The callable to register.
    ///     name: Optional name. Defaults to fn.__name__.
    ///     description: Optional description. Defaults to fn.__doc__ or empty string.
    ///     schema: Optional JSON Schema dict.
    #[pyo3(signature = (callable, *, name=None, description=None, schema=None))]
    fn add(
        &mut self,
        py: Python<'_>,
        callable: PyObject,
        name: Option<String>,
        description: Option<String>,
        schema: Option<Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        // Convert schema from Python to JSON Value if provided
        let schema_value: Option<Value> = schema
            .map(|s| pythonize::depythonize(&s))
            .transpose()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("Invalid schema: {e}")))?;

        let def = create_callback_def(py, callable, name, description, schema_value)?;
        self.callbacks.push(def);
        Ok(())
    }

    /// Return the number of registered callbacks.
    fn __len__(&self) -> usize {
        self.callbacks.len()
    }

    /// Return an iterator over the registered callbacks as dicts.
    fn __iter__(slf: PyRef<'_, Self>) -> PyResult<CallbackRegistryIter> {
        Ok(CallbackRegistryIter {
            callbacks: slf.callbacks.clone(),
            index: 0,
        })
    }

    fn __repr__(&self) -> String {
        let names: Vec<&str> = self.callbacks.iter().map(|c| c.name.as_str()).collect();
        format!("CallbackRegistry([{}])", names.join(", "))
    }
}

/// Helper class to act as a decorator for the `callback()` method.
#[pyclass]
struct DecoratorHelper {
    registry_ptr: usize,
    name: Option<String>,
    description: Option<String>,
    schema: Option<Value>,
}

#[pymethods]
impl DecoratorHelper {
    /// Called when the decorator is applied to a function.
    fn __call__(&self, py: Python<'_>, func: PyObject) -> PyResult<PyObject> {
        // SAFETY: The registry pointer is valid because the decorator is created
        // and used within the same Python scope where the registry exists.
        let registry = unsafe { &mut *(self.registry_ptr as *mut CallbackRegistry) };

        let def = create_callback_def(
            py,
            func.clone_ref(py),
            self.name.clone(),
            self.description.clone(),
            self.schema.clone(),
        )?;
        registry.callbacks.push(def);

        // Return the original function unchanged
        Ok(func)
    }
}

/// Iterator for CallbackRegistry.
#[pyclass]
struct CallbackRegistryIter {
    callbacks: Vec<CallbackDef>,
    index: usize,
}

#[pymethods]
impl CallbackRegistryIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> Option<PyObject> {
        if self.index < self.callbacks.len() {
            let def = &self.callbacks[self.index];
            self.index += 1;

            // Convert to a dict for Python consumption
            let dict = PyDict::new(py);
            dict.set_item("name", &def.name).ok()?;
            dict.set_item("fn", def.callable.clone_ref(py)).ok()?;
            dict.set_item("description", &def.description).ok()?;
            if let Some(schema) = &def.schema {
                let py_schema = pythonize::pythonize(py, schema).ok()?;
                dict.set_item("schema", py_schema).ok()?;
            }

            Some(dict.into_any().unbind())
        } else {
            None
        }
    }
}

/// Create a CallbackDef from Python objects, extracting name/description/schema as needed.
fn create_callback_def(
    py: Python<'_>,
    callable: PyObject,
    name: Option<String>,
    description: Option<String>,
    schema: Option<Value>,
) -> PyResult<CallbackDef> {
    // Get name from function if not provided
    let name = match name {
        Some(n) => n,
        None => callable
            .getattr(py, "__name__")
            .and_then(|n| n.extract::<String>(py))
            .unwrap_or_else(|_| "unknown".to_string()),
    };

    // Get description from docstring if not provided
    let description = match description {
        Some(d) => d,
        None => callable
            .getattr(py, "__doc__")
            .and_then(|d| d.extract::<Option<String>>(py))
            .unwrap_or(None)
            .map(|d| d.lines().next().unwrap_or("").trim().to_string())
            .unwrap_or_default(),
    };

    // Auto-infer schema if not provided
    let schema: Option<Value> = match schema {
        Some(s) => Some(s),
        None => infer_schema_from_callable(py, &callable)
            .ok()
            .and_then(|m| serde_json::to_value(m).ok()),
    };

    // Detect if this is an async function
    let is_async = detect_async_function(py, &callable)?;

    Ok(CallbackDef {
        name,
        description,
        callable,
        schema,
        is_async,
    })
}

/// Detect if a Python callable is an async function (coroutine function).
fn detect_async_function(py: Python<'_>, callable: &PyObject) -> PyResult<bool> {
    let inspect = py.import("inspect")?;
    let is_coro_func = inspect.call_method1("iscoroutinefunction", (callable,))?;
    is_coro_func.extract::<bool>()
}

/// Infer a JSON Schema from a Python callable's signature.
fn infer_schema_from_callable(
    py: Python<'_>,
    callable: &PyObject,
) -> PyResult<HashMap<String, Value>> {
    let inspect = py.import("inspect")?;
    let signature = inspect.call_method1("signature", (callable,))?;
    let parameters = signature.getattr("parameters")?;

    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    // Iterate over parameters
    let items = parameters.call_method0("items")?;
    let iter = items.try_iter()?;

    for item in iter {
        let item = item?;
        let tuple = item.downcast::<PyTuple>()?;
        let param_name: String = tuple.get_item(0)?.extract()?;
        let param = tuple.get_item(1)?;

        // Skip *args and **kwargs
        let kind = param.getattr("kind")?;
        let kind_name: String = kind.getattr("name")?.extract()?;
        if kind_name == "VAR_POSITIONAL" || kind_name == "VAR_KEYWORD" {
            continue;
        }

        // Get the annotation
        let annotation = param.getattr("annotation")?;
        let empty = inspect.getattr("Parameter")?.getattr("empty")?;

        let json_type = if annotation.is(&empty) {
            None
        } else {
            python_type_to_json_type(py, &annotation)
        };

        // Build property schema
        let mut prop_schema = serde_json::Map::new();
        if let Some(jt) = json_type {
            prop_schema.insert("type".to_string(), Value::String(jt));
        }
        properties.insert(param_name.clone(), Value::Object(prop_schema));

        // Check if parameter has a default
        let default = param.getattr("default")?;
        if default.is(&empty) {
            required.push(Value::String(param_name));
        }
    }

    let mut schema = HashMap::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }

    Ok(schema)
}

/// Map a Python type annotation to a JSON Schema type string.
fn python_type_to_json_type(py: Python<'_>, annotation: &Bound<'_, PyAny>) -> Option<String> {
    // Get the type's name
    let type_name = if let Ok(name) = annotation.getattr("__name__") {
        name.extract::<String>().ok()
    } else {
        // For generic types like list[str], get __origin__
        annotation
            .getattr("__origin__")
            .ok()
            .and_then(|origin| origin.getattr("__name__").ok())
            .and_then(|name| name.extract::<String>().ok())
    };

    let builtins = py.import("builtins").ok()?;

    // Check against builtin types
    let type_map: &[(&str, &str)] = &[
        ("str", "string"),
        ("int", "integer"),
        ("float", "number"),
        ("bool", "boolean"),
        ("list", "array"),
        ("dict", "object"),
        ("NoneType", "null"),
    ];

    if let Some(name) = type_name {
        for (py_type, json_type) in type_map {
            if name == *py_type {
                return Some((*json_type).to_string());
            }
        }
    }

    // Check if it's a typing generic (Optional, List, etc.)
    // For now, just check the origin
    if let Ok(origin) = annotation.getattr("__origin__") {
        // Check against builtins
        for (py_name, json_type) in type_map {
            if let Ok(builtin_type) = builtins.getattr(*py_name)
                && origin.is(&builtin_type)
            {
                return Some((*json_type).to_string());
            }
        }
    }

    None
}

/// Extract callbacks from various Python input types.
///
/// Accepts:
/// - A `CallbackRegistry` instance
/// - A list of dicts with "name", "fn", "description" keys
/// - A list of `CallbackRegistry` instances (merged)
/// - A mixed list
pub fn extract_callbacks(
    py: Python<'_>,
    callbacks: &Bound<'_, PyAny>,
) -> PyResult<Vec<PythonCallback>> {
    let mut result = Vec::new();

    // Check if it's a CallbackRegistry
    if let Ok(registry) = callbacks.extract::<PyRef<'_, CallbackRegistry>>() {
        for def in &registry.callbacks {
            result.push(callback_def_to_python_callback(py, def)?);
        }
        return Ok(result);
    }

    // Check if it's a list
    if let Ok(list) = callbacks.downcast::<PyList>() {
        for item in list.iter() {
            // Each item could be a dict or a CallbackRegistry
            if let Ok(registry) = item.extract::<PyRef<'_, CallbackRegistry>>() {
                for def in &registry.callbacks {
                    result.push(callback_def_to_python_callback(py, def)?);
                }
            } else if let Ok(dict) = item.downcast::<PyDict>() {
                result.push(dict_to_python_callback(py, dict)?);
            } else {
                return Err(pyo3::exceptions::PyTypeError::new_err(
                    "Callback list items must be dicts or CallbackRegistry instances",
                ));
            }
        }
        return Ok(result);
    }

    Err(pyo3::exceptions::PyTypeError::new_err(
        "callbacks must be a CallbackRegistry or list of callback dicts",
    ))
}

/// Convert a CallbackDef to a PythonCallback.
fn callback_def_to_python_callback(py: Python<'_>, def: &CallbackDef) -> PyResult<PythonCallback> {
    let schema = if let Some(schema_value) = &def.schema {
        Schema::try_from_value(schema_value.clone()).map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("Invalid JSON Schema: {e}"))
        })?
    } else {
        eryx::empty_schema()
    };

    Ok(PythonCallback::new(
        def.name.clone(),
        def.description.clone(),
        def.callable.clone_ref(py),
        schema,
        def.is_async,
    ))
}

/// Convert a Python dict to a PythonCallback.
fn dict_to_python_callback(py: Python<'_>, dict: &Bound<'_, PyDict>) -> PyResult<PythonCallback> {
    // Required: "name" and "fn"
    let name: String = dict
        .get_item("name")?
        .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("Callback dict missing 'name' key"))?
        .extract()?;

    let callable: PyObject = dict
        .get_item("fn")?
        .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("Callback dict missing 'fn' key"))?
        .extract()?;

    // Optional: "description"
    let description: String = dict
        .get_item("description")?
        .map(|d| d.extract())
        .transpose()?
        .unwrap_or_default();

    // Optional: "schema"
    let schema_value: Option<Value> = dict
        .get_item("schema")?
        .map(|s| pythonize::depythonize(&s))
        .transpose()
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("Invalid schema: {e}")))?;

    // If no schema provided, try to infer from callable
    let schema_value: Option<Value> = match schema_value {
        Some(s) => Some(s),
        None => infer_schema_from_callable(py, &callable)
            .ok()
            .and_then(|m| serde_json::to_value(m).ok()),
    };

    let schema = if let Some(schema_val) = schema_value {
        Schema::try_from_value(schema_val).map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("Invalid JSON Schema: {e}"))
        })?
    } else {
        eryx::empty_schema()
    };

    // Detect if this is an async function
    let is_async = detect_async_function(py, &callable)?;

    Ok(PythonCallback::new(
        name,
        description,
        callable,
        schema,
        is_async,
    ))
}
