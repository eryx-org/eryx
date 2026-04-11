//! ty type checker integration.
//!
//! Sets up a minimal salsa database with an in-memory filesystem to run
//! ty's type checker on a Python source string.

use std::sync::Arc;

use ruff_db::Db as SourceDb;
use ruff_db::files::{File, Files, system_path_to_file};
use ruff_db::system::{DbWithTestSystem, System, SystemPath, SystemPathBuf, TestSystem};
use ruff_db::vendored::VendoredFileSystem;
use ruff_python_ast::PythonVersion;
use ty_module_resolver::{Db as ModuleResolverDb, SearchPathSettings, SearchPaths};
use ty_python_semantic::Db as SemanticDb;
use ty_python_semantic::lint::{LintRegistry, RuleSelection};
use ty_python_semantic::types::check_types as ty_check_types;
use ty_python_semantic::{
    AnalysisSettings, FallibleStrategy, Program, ProgramSettings, PythonPlatform,
    PythonVersionSource, PythonVersionWithSource, default_lint_registry,
};

use crate::{Diagnostic, Severity, Source};

/// Salsa database for type checking.
#[salsa::db]
#[derive(Clone)]
struct CheckDb {
    storage: salsa::Storage<Self>,
    files: Files,
    system: TestSystem,
    vendored: VendoredFileSystem,
    rule_selection: Arc<RuleSelection>,
    analysis_settings: Arc<AnalysisSettings>,
}

impl CheckDb {
    fn new() -> Self {
        Self {
            storage: salsa::Storage::new(None),
            system: TestSystem::default(),
            vendored: ty_vendored::file_system().clone(),
            files: Files::default(),
            rule_selection: Arc::new(RuleSelection::from_registry(default_lint_registry())),
            analysis_settings: AnalysisSettings::default().into(),
        }
    }
}

impl DbWithTestSystem for CheckDb {
    fn test_system(&self) -> &TestSystem {
        &self.system
    }

    fn test_system_mut(&mut self) -> &mut TestSystem {
        &mut self.system
    }
}

#[salsa::db]
impl SourceDb for CheckDb {
    fn vendored(&self) -> &VendoredFileSystem {
        &self.vendored
    }

    fn system(&self) -> &dyn System {
        &self.system
    }

    fn files(&self) -> &Files {
        &self.files
    }

    fn python_version(&self) -> PythonVersion {
        Program::get(self).python_version(self)
    }
}

#[salsa::db]
impl SemanticDb for CheckDb {
    fn should_check_file(&self, file: File) -> bool {
        !file.path(self).is_vendored_path()
    }

    fn rule_selection(&self, _file: File) -> &RuleSelection {
        &self.rule_selection
    }

    fn lint_registry(&self) -> &LintRegistry {
        default_lint_registry()
    }

    fn analysis_settings(&self, _file: File) -> &AnalysisSettings {
        &self.analysis_settings
    }

    fn verbose(&self) -> bool {
        false
    }
}

#[salsa::db]
impl ModuleResolverDb for CheckDb {
    fn search_paths(&self) -> &SearchPaths {
        Program::get(self).search_paths(self)
    }
}

#[salsa::db]
impl salsa::Database for CheckDb {}

/// Type-check a Python source string using ty.
pub(crate) fn check(source: &str) -> anyhow::Result<Vec<Diagnostic>> {
    let db = CheckDb::new();

    let src_root = SystemPathBuf::from("/src");
    db.memory_file_system().create_directory_all(&src_root)?;
    db.memory_file_system()
        .write_file_all(SystemPath::new("/src/script.py"), source)?;

    Program::from_settings(
        &db,
        ProgramSettings {
            python_version: PythonVersionWithSource {
                version: PythonVersion::default(),
                source: PythonVersionSource::default(),
            },
            python_platform: PythonPlatform::default(),
            search_paths: SearchPathSettings::new(vec![src_root]).to_search_paths(
                db.system(),
                db.vendored(),
                &FallibleStrategy,
            )?,
        },
    );

    let file = system_path_to_file(&db, "/src/script.py")?;
    let ty_diagnostics = ty_check_types(&db, file);

    let diagnostics = ty_diagnostics
        .into_iter()
        .map(|d| {
            let severity = match d.severity() {
                ruff_db::diagnostic::Severity::Info => Severity::Info,
                ruff_db::diagnostic::Severity::Warning => Severity::Warning,
                ruff_db::diagnostic::Severity::Error | ruff_db::diagnostic::Severity::Fatal => {
                    Severity::Error
                }
            };

            let (start, end) = d
                .range()
                .map(|r| (u32::from(r.start()), u32::from(r.end())))
                .unwrap_or((0, 0));

            Diagnostic {
                message: d.primary_message().to_string(),
                severity,
                source: Source::Type,
                start_offset: start,
                end_offset: end,
            }
        })
        .collect();

    Ok(diagnostics)
}
