// Examples use expect/unwrap for simplicity
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Example demonstrating sandboxed Jinja2 template evaluation.
//!
//! This shows how to safely evaluate user-supplied Jinja2 templates
//! inside the WebAssembly sandbox, preventing any access to the host system.
//!
//! Uses pre-initialization to bake jinja2 imports into the WASM component,
//! so each sandbox starts with jinja2 already loaded (~10ms vs ~670ms).
//!
//! # Prerequisites
//!
//! Download jinja2 (pure Python) and markupsafe (WASI-compiled):
//! ```bash
//! pip download --only-binary=:all: --dest /tmp/wheels jinja2
//! cp ~/repos/misc/wasi-wheels/packages/markupsafe-3.0.2/dist/markupsafe-3.0.2-cp313-cp313-wasi_0_0_0_wasm32.whl /tmp/wheels/
//! ```
//!
//! # Running
//!
//! ```bash
//! cargo run --example jinja2_sandbox --features native-extensions,preinit,embedded --release
//! ```

use std::path::Path;
use std::time::Instant;

use eryx::Sandbox;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== Jinja2 Template Sandbox Example ===\n");

    // Check for required wheel files
    let wheels_dir = Path::new("/tmp/wheels");
    if !wheels_dir.exists() {
        print_download_instructions();
        return Ok(());
    }

    // Find the wheel files
    let jinja2_wheel = match find_wheel(wheels_dir, "jinja2") {
        Ok(p) => p,
        Err(_) => {
            print_download_instructions();
            return Ok(());
        }
    };

    let markupsafe_wheel = match find_wheel(wheels_dir, "markupsafe") {
        Ok(p) => p,
        Err(_) => {
            print_download_instructions();
            return Ok(());
        }
    };

    println!("Found wheels:");
    println!("  - {}", jinja2_wheel.display());
    println!("  - {}", markupsafe_wheel.display());
    println!();

    // Extract wheels to a site-packages directory
    let site_packages = extract_wheels(&[&jinja2_wheel, &markupsafe_wheel])?;
    println!("Extracted to: {}\n", site_packages.display());

    // Get Python stdlib path from the embedded runtime
    let embedded = eryx::embedded::EmbeddedResources::get()?;
    let python_stdlib = embedded.stdlib().to_path_buf();

    // Load native extensions from markupsafe
    let extensions = load_extensions(&site_packages)?;
    println!("Loaded {} native extension(s)\n", extensions.len());

    // Pre-initialize the component with jinja2 already imported
    println!("--- Pre-initializing with jinja2 ---");
    let start = Instant::now();
    let preinit_component = eryx::preinit::pre_initialize(
        &python_stdlib,
        Some(&site_packages),
        &["jinja2"], // Pre-import jinja2 during init
        &extensions,
    )
    .await?;
    println!(
        "  Pre-initialized in {:?} ({:.1} MB component)",
        start.elapsed(),
        preinit_component.len() as f64 / 1_000_000.0
    );

    // Pre-compile for faster instantiation
    let start = Instant::now();
    let precompiled = eryx::PythonExecutor::precompile(&preinit_component)?;
    println!(
        "  Pre-compiled in {:?} ({:.1} MB)",
        start.elapsed(),
        precompiled.len() as f64 / 1_000_000.0
    );

    // Helper to create a sandbox from the pre-initialized component
    let create_sandbox = || unsafe {
        Sandbox::builder()
            .with_precompiled_bytes(precompiled.clone())
            .with_python_stdlib(&python_stdlib)
            .with_site_packages(&site_packages)
            .build()
    };

    // Example 1: Simple template rendering
    println!("\n=== Example 1: Simple template ===");
    let start = Instant::now();
    let sandbox = create_sandbox()?;
    let sandbox_time = start.elapsed();

    let start = Instant::now();
    let result = sandbox
        .execute(
            r#"
from jinja2 import Template

template = Template("Hello, {{ name }}!")
output = template.render(name="World")
print(output)
"#,
        )
        .await?;
    println!(
        "  Sandbox: {:?}, Execute: {:?}",
        sandbox_time,
        start.elapsed()
    );
    println!("  Output: {}", result.stdout.trim());

    // Example 2: Template with loops and conditionals
    println!("\n=== Example 2: Loops and conditionals ===");
    let start = Instant::now();
    let sandbox = create_sandbox()?;
    let sandbox_time = start.elapsed();

    let start = Instant::now();
    let result = sandbox
        .execute(
            r#"
from jinja2 import Template

template_str = """
{% for item in items %}
  - {{ item.name }}: {% if item.active %}ACTIVE{% else %}inactive{% endif %}
{% endfor %}
"""

template = Template(template_str)
output = template.render(items=[
    {"name": "Server A", "active": True},
    {"name": "Server B", "active": False},
    {"name": "Server C", "active": True},
])
print(output)
"#,
        )
        .await?;
    println!(
        "  Sandbox: {:?}, Execute: {:?}",
        sandbox_time,
        start.elapsed()
    );
    println!("  Output:{}", result.stdout.trim_end());

    // Example 3: User-supplied template (simulating untrusted input)
    println!("\n=== Example 3: User-supplied template ===");

    let user_template = r#"
Report for {{ company }}
========================
{% for dept in departments %}
Department: {{ dept.name }}
  Budget: ${{ "{:,.2f}".format(dept.budget) }}
  Employees: {{ dept.employees|length }}
{% endfor %}
Total Budget: ${{ "{:,.2f}".format(departments|sum(attribute='budget')) }}
"#;

    let user_data = r#"
{
    "company": "Acme Corp",
    "departments": [
        {"name": "Engineering", "budget": 500000, "employees": ["Alice", "Bob", "Charlie"]},
        {"name": "Marketing", "budget": 200000, "employees": ["Diana", "Eve"]},
        {"name": "Operations", "budget": 150000, "employees": ["Frank"]}
    ]
}
"#;

    let code = format!(
        r#"
import json
from jinja2 import Template

template_str = '''{user_template}'''
data = json.loads('''{user_data}''')

template = Template(template_str)
output = template.render(**data)
print(output)
"#
    );

    let start = Instant::now();
    let sandbox = create_sandbox()?;
    let sandbox_time = start.elapsed();

    let start = Instant::now();
    let result = sandbox.execute(&code).await?;
    println!(
        "  Sandbox: {:?}, Execute: {:?}",
        sandbox_time,
        start.elapsed()
    );
    println!("  Output:{}", result.stdout.trim_end());

    // Example 4: Environment with custom filters
    println!("\n=== Example 4: Environment with custom filters ===");
    let start = Instant::now();
    let sandbox = create_sandbox()?;
    let sandbox_time = start.elapsed();

    let start = Instant::now();
    let result = sandbox
        .execute(
            r#"
from jinja2 import Environment

env = Environment()

def reverse_string(s):
    return s[::-1]

env.filters['reverse'] = reverse_string

template = env.from_string("Original: {{ text }}\nReversed: {{ text|reverse }}")
output = template.render(text="Hello, Jinja2!")
print(output)
"#,
        )
        .await?;
    println!(
        "  Sandbox: {:?}, Execute: {:?}",
        sandbox_time,
        start.elapsed()
    );
    println!("  Output:\n{}", indent(&result.stdout, "    "));

    // Example 5: Template inheritance
    println!("=== Example 5: Template inheritance (in-memory) ===");
    let start = Instant::now();
    let sandbox = create_sandbox()?;
    let sandbox_time = start.elapsed();

    let start = Instant::now();
    let result = sandbox
        .execute(
            r#"
from jinja2 import Environment, DictLoader

templates = {
    'base.html': '''
<!DOCTYPE html>
<html>
<head><title>{% block title %}{% endblock %}</title></head>
<body>
  <header>{{ site_name }}</header>
  <main>{% block content %}{% endblock %}</main>
  <footer>Copyright 2024</footer>
</body>
</html>
''',
    'page.html': '''
{% extends "base.html" %}
{% block title %}{{ page_title }}{% endblock %}
{% block content %}
<h1>{{ page_title }}</h1>
<p>{{ content }}</p>
{% endblock %}
'''
}

env = Environment(loader=DictLoader(templates))
template = env.get_template('page.html')
output = template.render(
    site_name="My Site",
    page_title="Welcome",
    content="This is the page content."
)
print(output)
"#,
        )
        .await?;
    println!(
        "  Sandbox: {:?}, Execute: {:?}",
        sandbox_time,
        start.elapsed()
    );
    println!("  Output:\n{}", indent(result.stdout.trim(), "    "));

    // Example 6: Sandbox security
    println!("\n=== Example 6: Security - sandbox isolation ===");
    let start = Instant::now();
    let sandbox = create_sandbox()?;
    let sandbox_time = start.elapsed();

    let start = Instant::now();
    let result = sandbox
        .execute(
            r#"
from jinja2 import Template

template = Template("{{ config }}")
try:
    output = template.render(config="Safe value")
    print(f"Rendered safely: {output}")
except Exception as e:
    print(f"Caught: {e}")

import os
try:
    files = os.listdir("/")
    print(f"Root contains: {files}")
except Exception as e:
    print(f"File access error: {e}")
"#,
        )
        .await?;
    println!(
        "  Sandbox: {:?}, Execute: {:?}",
        sandbox_time,
        start.elapsed()
    );
    println!("  Output:\n{}", indent(&result.stdout, "    "));

    println!("=== Summary ===");
    println!("  Pre-initialization bakes jinja2 into the WASM component");
    println!("  Each sandbox creation: ~10ms (vs ~670ms without pre-init)");
    println!("  Full isolation: each sandbox is independent");

    Ok(())
}

fn print_download_instructions() {
    eprintln!("Wheels not found at /tmp/wheels");
    eprintln!();
    eprintln!("Download jinja2 and markupsafe with:");
    eprintln!("  pip download --only-binary=:all: --dest /tmp/wheels jinja2");
    eprintln!(
        "  cp ~/repos/misc/wasi-wheels/packages/markupsafe-3.0.2/dist/markupsafe-3.0.2-cp313-cp313-wasi_0_0_0_wasm32.whl /tmp/wheels/"
    );
}

/// Find a wheel file in a directory by package name prefix.
fn find_wheel(dir: &Path, package: &str) -> anyhow::Result<std::path::PathBuf> {
    let prefix = format!("{}-", package.replace('-', "_"));
    let prefix_alt = format!("{}-", package);

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_lowercase();

        if name_str.ends_with(".whl")
            && (name_str.starts_with(&prefix.to_lowercase())
                || name_str.starts_with(&prefix_alt.to_lowercase()))
        {
            return Ok(entry.path());
        }
    }

    anyhow::bail!("Could not find {package} wheel in {}", dir.display())
}

/// Extract wheel files to a temporary site-packages directory.
fn extract_wheels(wheels: &[&Path]) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Read;

    let site_packages = std::env::temp_dir().join("eryx-jinja2-site-packages");
    if site_packages.exists() {
        std::fs::remove_dir_all(&site_packages)?;
    }
    std::fs::create_dir_all(&site_packages)?;

    for wheel_path in wheels {
        let file = std::fs::File::open(wheel_path)?;
        let mut archive = zip::ZipArchive::new(file)?;

        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let outpath = site_packages.join(file.name());

            if file.is_dir() {
                std::fs::create_dir_all(&outpath)?;
            } else {
                if let Some(parent) = outpath.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut outfile = std::fs::File::create(&outpath)?;
                let mut contents = Vec::new();
                file.read_to_end(&mut contents)?;
                std::io::Write::write_all(&mut outfile, &contents)?;
            }
        }
    }

    Ok(site_packages)
}

/// Load native extensions (.so files) from a site-packages directory.
fn load_extensions(site_packages: &Path) -> anyhow::Result<Vec<eryx::preinit::NativeExtension>> {
    let mut extensions = Vec::new();

    for entry in walkdir::WalkDir::new(site_packages) {
        let entry = entry?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "so") {
            let relative = path.strip_prefix(site_packages)?;
            let dlopen_path = format!("/site-packages/{}", relative.display());
            let bytes = std::fs::read(path)?;
            extensions.push(eryx::preinit::NativeExtension::new(dlopen_path, bytes));
        }
    }

    Ok(extensions)
}

fn indent(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
