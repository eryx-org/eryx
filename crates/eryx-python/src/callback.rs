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
//!
//! # Async Callback Limitations
//!
//! Async callbacks are supported, but they run in **isolated event loops**. This means:
//!
//! - Each callback invocation creates its own `asyncio` event loop via `asyncio.run()`
//! - Callbacks **cannot** share `asyncio`-bound resources with the parent application
//!   (e.g., `asyncio.Queue`, `asyncio.Lock`, `asyncio.Semaphore`, `aiohttp.ClientSession`)
//! - Multiple concurrent callbacks run in parallel via tokio, but each has its own event loop
//!
//! ## Why This Limitation Exists
//!
//! This is a fundamental architectural constraint:
//!
//! 1. **Thread isolation**: Callbacks execute on tokio's blocking thread pool, not the
//!    caller's thread. Python's `asyncio` event loops are thread-specific.
//!
//! 2. **GIL constraints**: Even with `pyo3-async-runtimes`, coordinating multiple Rust
//!    futures that share a Python event loop is complex due to GIL acquisition timing.
//!
//! 3. **Cross-thread asyncio**: `asyncio.run_coroutine_threadsafe()` can schedule work
//!    on another thread's event loop, but primitives like `Semaphore` that require
//!    waiters and releasers to coordinate still fail when callbacks wait on each other.
//!
//! ## Workarounds
//!
//! For shared state between callbacks, use **thread-safe** primitives:
//!
//! ```python
//! import threading
//! import queue
//!
//! # ✅ Thread-safe - works across callbacks
//! shared_queue = queue.Queue()
//! shared_lock = threading.Lock()
//! shared_counter = {"value": 0}
//!
//! async def my_callback(item: str):
//!     with shared_lock:
//!         shared_counter["value"] += 1
//!     shared_queue.put(item)
//!     return {"count": shared_counter["value"]}
//!
//! # ❌ NOT thread-safe - will fail or deadlock
//! async_queue = asyncio.Queue()  # Bound to wrong event loop
//! async_lock = asyncio.Lock()    # Bound to wrong event loop
//! ```
//!
//! ## Potential Future Improvements
//!
//! We may address this limitation in the future through one of these approaches:
//!
//! 1. **Thread pinning**: Run all callbacks on a dedicated Python thread with a
//!    long-lived event loop, using a channel to dispatch work from tokio.
//!
//! 2. **`LocalSet` integration**: Use tokio's `LocalSet` to run callbacks on the
//!    same thread as a shared Python event loop.
//!
//! 3. **User-provided event loop**: Allow users to pass an existing event loop
//!    reference that callbacks should integrate with.
//!
//! 4. **Explicit opt-in**: Provide a `shared_loop=True` option on `CallbackRegistry`
//!    that enables event loop sharing for users who need it, with clear documentation
//!    of the additional complexity and potential pitfalls.
//!
//! If you have a use case that requires shared event loop resources, please open an
//! issue at <https://github.com/eryx-org/eryx/issues>.

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
// We always use `Python::attach()` when accessing the callable.
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
        let callable = Python::attach(|py| self.callable.clone_ref(py));
        let is_async = self.is_async;

        Box::pin(async move {
            // Use spawn_blocking to avoid blocking the tokio runtime while holding the GIL.
            // This applies to both sync and async callbacks.
            //
            // For async callbacks, we use asyncio.run() which creates an isolated event loop.
            // This means callbacks cannot share event-loop-bound resources (like asyncio.Queue,
            // asyncio.Semaphore, etc.) with the parent application. This is a fundamental
            // limitation because:
            // 1. Callbacks run on tokio's blocking thread pool, not the caller's thread
            // 2. asyncio event loops are thread-specific
            // 3. pyo3-async-runtimes' into_future() uses run_coroutine_threadsafe which
            //    still creates coordination issues for cross-callback synchronization
            //
            // For use cases requiring shared async state, users should use thread-safe
            // primitives (threading.Lock, queue.Queue) or design callbacks to be stateless.
            tokio::task::spawn_blocking(move || {
                Python::attach(|py| {
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
///
/// This creates an isolated event loop for the coroutine. Each callback
/// invocation gets its own event loop, which means callbacks cannot share
/// event-loop-bound resources with each other or the parent application.
fn run_coroutine(py: Python<'_>, coro: &Bound<'_, PyAny>) -> Result<Py<PyAny>, CallbackError> {
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
/// Callbacks are Python functions that can be invoked from sandboxed code.
/// Both sync and async functions are supported.
///
/// Example:
///     registry = eryx.CallbackRegistry()
///
///     @registry.callback(description="Returns current timestamp")
///     def get_time():
///         import time
///         return {"timestamp": time.time()}
///
///     @registry.callback(description="Async greeting")
///     async def greet(name: str):
///         return {"greeting": f"Hello, {name}!"}
///
///     sandbox = eryx.Sandbox(callbacks=registry)
///
/// Important - Async Callback Limitations:
///     Async callbacks run in **isolated event loops**. Each callback invocation
///     creates its own event loop via `asyncio.run()`. This means:
///
///     - Callbacks CANNOT share asyncio-bound resources with the parent application
///       (e.g., `asyncio.Queue`, `asyncio.Lock`, `asyncio.Semaphore`, `aiohttp.ClientSession`)
///     - Multiple concurrent callbacks run in parallel via tokio, but each has its own loop
///
///     This is a fundamental constraint because callbacks execute on tokio's blocking
///     thread pool, and Python's asyncio event loops are thread-specific.
///
///     Workaround - use thread-safe primitives for shared state:
///
///         import threading
///         import queue
///
///         # ✅ Thread-safe - works across callbacks
///         shared_queue = queue.Queue()
///         shared_lock = threading.Lock()
///
///         @registry.callback()
///         async def my_callback(item: str):
///             with shared_lock:
///                 shared_queue.put(item)
///             return {"queued": item}
///
///         # ❌ Will NOT work - bound to wrong event loop
///         async_queue = asyncio.Queue()
///
///     Future improvements under consideration:
///         - Thread pinning with a dedicated Python event loop thread
///         - tokio LocalSet integration for same-thread execution
///         - User-provided event loop reference
///         - Opt-in `shared_loop=True` mode
///
///     See: https://github.com/eryx-org/eryx/issues for updates.
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
        Python::attach(|py| Self {
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
    /// Both sync and async functions are supported. Async functions will be
    /// executed using `asyncio.run()`, which creates an isolated event loop
    /// for each invocation. See the class docstring for important limitations
    /// regarding async callbacks and shared state.
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
    ///
    ///     @registry.callback(description="Async fetch")
    ///     async def fetch_data(url: str):
    ///         # Note: Cannot share aiohttp.ClientSession with parent app
    ///         async with aiohttp.ClientSession() as session:
    ///             async with session.get(url) as resp:
    ///                 return {"status": resp.status}
    #[pyo3(signature = (name=None, description=None, schema=None))]
    fn callback(
        slf: PyRefMut<'_, Self>,
        name: Option<String>,
        description: Option<String>,
        schema: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let py = slf.py();

        // Convert schema from Python to JSON Value if provided
        let schema_value: Option<Value> = schema
            .map(|s| pythonize::depythonize(&s))
            .transpose()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("Invalid schema: {e}")))?;

        // We need to create a Python function that acts as a decorator
        // Since we can't easily create closures, we'll use a helper class
        let decorator = DecoratorHelper {
            registry: slf.into(),
            name,
            description,
            schema: schema_value,
        };

        Ok(decorator.into_pyobject(py)?.into_any().unbind())
    }

    /// Add a callback directly without using the decorator pattern.
    ///
    /// Both sync and async functions are supported. See the class docstring for
    /// important limitations regarding async callbacks and shared state.
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
        callable: Py<PyAny>,
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
    registry: Py<CallbackRegistry>,
    name: Option<String>,
    description: Option<String>,
    schema: Option<Value>,
}

#[pymethods]
impl DecoratorHelper {
    /// Called when the decorator is applied to a function.
    fn __call__(&self, py: Python<'_>, func: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let mut registry = self.registry.borrow_mut(py);

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

    fn __next__(&mut self, py: Python<'_>) -> Option<Py<PyAny>> {
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
    callable: Py<PyAny>,
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
fn detect_async_function(py: Python<'_>, callable: &Py<PyAny>) -> PyResult<bool> {
    let inspect = py.import("inspect")?;
    let is_coro_func = inspect.call_method1("iscoroutinefunction", (callable,))?;
    is_coro_func.extract::<bool>()
}

/// Infer a JSON Schema from a Python callable's signature.
fn infer_schema_from_callable(
    py: Python<'_>,
    callable: &Py<PyAny>,
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
        let tuple: &Bound<'_, PyTuple> = item.cast()?;
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
    if let Ok(list) = callbacks.cast::<PyList>() {
        for item in list.iter() {
            // Each item could be a dict or a CallbackRegistry
            if let Ok(registry) = item.extract::<PyRef<'_, CallbackRegistry>>() {
                for def in &registry.callbacks {
                    result.push(callback_def_to_python_callback(py, def)?);
                }
            } else if let Ok(dict) = item.cast::<PyDict>() {
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

    let callable: Py<PyAny> = dict
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
