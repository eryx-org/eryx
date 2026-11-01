//! The file format for a precompiled runtime that can be rebuilt.
//!
//! A precompiled component only loads in the eryx version that built it (see
//! [`Error::IncompatiblePrecompiled`](crate::Error::IncompatiblePrecompiled)).
//! A bundle stores what the component was built from next to it, so another
//! eryx version can build it again. `eryx-precompile compile --preinit` and
//! pyeryx's `SandboxFactory.save()` write bundles, and `eryx-precompile
//! upgrade` and `SandboxFactory.load(rebuild_if_incompatible=True)` rebuild
//! them.
//!
//! ```text
//! [component_len bytes: precompiled component, starting at offset 0]
//! [data_files_len bytes: zstd-compressed tar of the site-packages files]
//! [build_inputs_len bytes: JSON build inputs, see `BuildInputs`]
//! [8 bytes: component_len (little-endian)]
//! [8 bytes: data_files_len (little-endian)]
//! [8 bytes: build_inputs_len (little-endian)]
//! [32 bytes: component content hash, for the instance-pre cache key]
//! [4 bytes: version (4, little-endian)]
//! [8 bytes: magic "ERYXFCT4"]
//! ```
//!
//! The component comes first because wasmtime only memory-maps a component
//! that starts at offset 0 of a file, and it ignores the bytes after it. So
//! anything that loads a plain precompiled file, such as
//! [`SandboxBuilder::with_precompiled_file`](crate::SandboxBuilder::with_precompiled_file)
//! or the server's `--runtime-cwasm`, loads a bundle too.
//!
//! The data files are the site-packages without `.pyc` files, which the
//! interpreter recompiles. An empty build inputs section means the bundle
//! can't be rebuilt.
//!
//! Version 3, written by unreleased builds of pyeryx, is version 4 without
//! the build inputs, with footer `[component_len u64][data_files_len u64]
//! [hash 32][version 3 u32][magic "ERYXFCT3"]`, and without `.py` files in its
//! data files. Version 2 is pyeryx's legacy envelope, `[magic "ERYX"]
//! [version 2 u32][component_len u64][component][data_files_len u64]
//! [data files]`. A file with neither footer nor the v2 magic is a bare
//! component (version 1).

use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::Path;

use crate::CpuFeatureLevel;
use crate::preinit::CallbackDeclaration;

/// The magic at the start of a legacy v2 file.
pub const V2_MAGIC: &[u8; 4] = b"ERYX";
const FOOTER_MAGIC: &[u8; 8] = b"ERYXFCT4";
const FOOTER_LEN: u64 = 8 + 8 + 8 + 32 + 4 + 8;
const VERSION: u32 = 4;
const V3_FOOTER_MAGIC: &[u8; 8] = b"ERYXFCT3";
const V3_FOOTER_LEN: u64 = 8 + 8 + 32 + 4 + 8;

/// What a bundle's component was built from, apart from its site-packages
/// files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildInputs {
    /// Modules imported during pre-initialization.
    pub imports: Vec<String>,
    /// Code run during pre-initialization, after the imports.
    pub setup_code: Option<String>,
    /// Callback declarations baked into the snapshot.
    pub callbacks: Vec<CallbackDeclaration>,
    /// The target triple compiled for, or `None` for the host.
    pub target: Option<String>,
    /// The CPU feature level compiled for, or `None` for the default.
    pub cpu_features: Option<CpuFeatureLevel>,
}

impl BuildInputs {
    /// Build inputs with nothing set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the modules imported during pre-initialization.
    #[must_use]
    pub fn with_imports(mut self, imports: impl IntoIterator<Item = String>) -> Self {
        self.imports = imports.into_iter().collect();
        self
    }

    /// Set the code run during pre-initialization.
    #[must_use]
    pub fn with_setup_code(mut self, setup_code: Option<String>) -> Self {
        self.setup_code = setup_code;
        self
    }

    /// Set the callback declarations baked into the snapshot.
    #[must_use]
    pub fn with_callbacks(mut self, callbacks: Vec<CallbackDeclaration>) -> Self {
        self.callbacks = callbacks;
        self
    }

    /// Set the target triple compiled for.
    #[must_use]
    pub fn with_target(mut self, target: Option<String>) -> Self {
        self.target = target;
        self
    }

    /// Set the CPU feature level compiled for.
    #[must_use]
    pub fn with_cpu_features(mut self, cpu_features: Option<CpuFeatureLevel>) -> Self {
        self.cpu_features = cpu_features;
        self
    }

    /// Encode as the JSON stored in a bundle.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> std::io::Result<Vec<u8>> {
        let callbacks: Vec<_> = self
            .callbacks
            .iter()
            .map(|cb| {
                serde_json::json!({
                    "name": cb.name,
                    "description": cb.description,
                    "parameters_schema_json": cb.parameters_schema_json,
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "imports": self.imports,
            "setup_code": self.setup_code,
            "callbacks": callbacks,
            "target": self.target,
            "cpu_features": self.cpu_features.map(|level| level.as_str()),
        }))
        .map_err(std::io::Error::other)
    }

    /// Decode the JSON stored in a bundle.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::ErrorKind::InvalidData`] if `json` isn't valid
    /// build inputs.
    pub fn from_json(json: &[u8]) -> std::io::Result<Self> {
        let invalid = |msg: &str| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid build inputs: {msg}"),
            )
        };
        let manifest: serde_json::Value =
            serde_json::from_slice(json).map_err(|e| invalid(&e.to_string()))?;

        let string = |value: &serde_json::Value, name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| invalid(&format!("`{name}` is not a string")))
        };
        let optional_string = |name: &str| match manifest.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(_) => string(&manifest, name).map(Some),
        };
        let array = |name: &str| {
            manifest
                .get(name)
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| invalid(&format!("`{name}` is not an array")))
        };

        let imports = array("imports")?
            .iter()
            .map(|import| {
                import
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("an import is not a string"))
            })
            .collect::<Result<_, _>>()?;
        let callbacks = array("callbacks")?
            .iter()
            .map(|cb| {
                Ok(CallbackDeclaration {
                    name: string(cb, "name")?,
                    description: string(cb, "description")?,
                    parameters_schema_json: string(cb, "parameters_schema_json")?,
                })
            })
            .collect::<std::io::Result<_>>()?;
        let cpu_features = optional_string("cpu_features")?
            .map(|level| {
                CpuFeatureLevel::parse(&level)
                    .ok_or_else(|| invalid(&format!("unknown CPU feature level `{level}`")))
            })
            .transpose()?;

        Ok(Self {
            imports,
            setup_code: optional_string("setup_code")?,
            callbacks,
            target: optional_string("target")?,
            cpu_features,
        })
    }
}

/// How a bundle or older precompiled file is laid out, from [`read_layout`].
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Layout {
    /// The component is the first `component_len` bytes; the data files
    /// archive and the build inputs follow it, each absent if its length is
    /// zero. Bundles (v3, v4) carry the component's content hash; a bare
    /// component (v1) doesn't.
    Mapped {
        /// Length of the component at the start of the file.
        component_len: u64,
        /// Length of the data files archive after it.
        data_files_len: u64,
        /// Length of the build inputs after that.
        build_inputs_len: u64,
        /// The component's content hash, if the file records one.
        component_hash: Option<[u8; 32]>,
    },
    /// pyeryx's legacy v2 envelope, which has to be read into memory.
    V2,
}

impl Layout {
    /// The byte ranges of the data files archive and the build inputs, for a
    /// mapped layout.
    #[must_use]
    pub fn sections(&self) -> Option<(Range<u64>, Range<u64>)> {
        match *self {
            Self::Mapped {
                component_len,
                data_files_len,
                build_inputs_len,
                ..
            } => {
                let data_files_end = component_len + data_files_len;
                Some((
                    component_len..data_files_end,
                    data_files_end..data_files_end + build_inputs_len,
                ))
            }
            Self::V2 => None,
        }
    }
}

/// Work out a file's layout from its footer and header.
///
/// # Errors
///
/// Returns an error if reading fails, or
/// [`std::io::ErrorKind::InvalidData`] if a bundle footer is inconsistent.
pub fn read_layout(file: &mut (impl Read + Seek)) -> std::io::Result<Layout> {
    let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);
    let file_len = file.seek(SeekFrom::End(0))?;

    let mut magic = [0u8; 8];
    if file_len >= 8 {
        file.seek(SeekFrom::End(-8))?;
        file.read_exact(&mut magic)?;
    }
    // (footer length, expected version, whether it records build inputs)
    let footer = match &magic {
        m if m == FOOTER_MAGIC => Some((FOOTER_LEN, VERSION, true)),
        m if m == V3_FOOTER_MAGIC => Some((V3_FOOTER_LEN, 3, false)),
        _ => None,
    };
    if let Some((footer_len, expected_version, has_build_inputs)) = footer
        && file_len >= footer_len
    {
        let mut footer = vec![0u8; usize::try_from(footer_len).unwrap_or(0)];
        file.seek(SeekFrom::End(-(footer_len as i64)))?;
        file.read_exact(&mut footer)?;
        let version_at = footer.len() - 12;
        let version = read_le_u32(&footer[version_at..version_at + 4])?;
        if version != expected_version {
            return Err(invalid(format!(
                "unsupported precompiled file version: {version}"
            )));
        }
        let component_len = read_le_u64(&footer[..8])?;
        let data_files_len = read_le_u64(&footer[8..16])?;
        let (build_inputs_len, hash_at) = if has_build_inputs {
            (read_le_u64(&footer[16..24])?, 24)
        } else {
            (0, 16)
        };
        let expected = component_len
            .checked_add(data_files_len)
            .and_then(|len| len.checked_add(build_inputs_len))
            .and_then(|len| len.checked_add(footer_len));
        if expected != Some(file_len) {
            return Err(invalid(format!(
                "precompiled file is {file_len} bytes but its footer describes \
                 {component_len} + {data_files_len} + {build_inputs_len} + {footer_len}"
            )));
        }
        let mut component_hash = [0u8; 32];
        component_hash.copy_from_slice(&footer[hash_at..hash_at + 32]);
        return Ok(Layout::Mapped {
            component_len,
            data_files_len,
            build_inputs_len,
            component_hash: Some(component_hash),
        });
    }

    let mut magic = [0u8; 4];
    file.seek(SeekFrom::Start(0))?;
    if file_len >= 4 {
        file.read_exact(&mut magic)?;
    }
    if &magic == V2_MAGIC {
        Ok(Layout::V2)
    } else {
        // v1: the whole file is the component.
        Ok(Layout::Mapped {
            component_len: file_len,
            data_files_len: 0,
            build_inputs_len: 0,
            component_hash: None,
        })
    }
}

/// Write a v4 bundle to `writer`.
///
/// `build_inputs` is [`BuildInputs::to_json`]'s output, or empty for a
/// bundle that can't be rebuilt.
///
/// # Errors
///
/// Returns an error if reading the component or writing fails.
pub fn write(
    precompiled: &crate::PrecompiledArtifact,
    data_files: &[(String, Vec<u8>)],
    build_inputs: &[u8],
    writer: &mut impl Write,
) -> std::io::Result<()> {
    let data_files_archive = archive_data_files(data_files)?;
    let component_hash = precompiled.content_hash()?;
    precompiled.write_to(writer)?;
    writer.write_all(&data_files_archive)?;
    writer.write_all(build_inputs)?;
    writer.write_all(&(precompiled.len() as u64).to_le_bytes())?;
    writer.write_all(&(data_files_archive.len() as u64).to_le_bytes())?;
    writer.write_all(&(build_inputs.len() as u64).to_le_bytes())?;
    writer.write_all(&component_hash)?;
    writer.write_all(&VERSION.to_le_bytes())?;
    writer.write_all(FOOTER_MAGIC)?;
    Ok(())
}

/// Collect the files of a site-packages directory to store in a bundle, as
/// (relative_path, contents) pairs.
///
/// `.py` files are kept: the snapshot only holds the modules imported during
/// pre-initialization, so the others are imported from these files. `.pyc`
/// files are skipped, since the interpreter recompiles them.
///
/// # Errors
///
/// Returns an error if walking or reading the directory fails.
pub fn collect_data_files(site_packages: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    if !site_packages.exists() {
        return Ok(files);
    }
    for entry in walkdir::WalkDir::new(site_packages) {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || path.extension().is_some_and(|ext| ext == "pyc") {
            continue;
        }
        let relative = path
            .strip_prefix(site_packages)
            .map_err(std::io::Error::other)?;
        let relative_str = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        files.push((relative_str, std::fs::read(path)?));
    }
    Ok(files)
}

/// A zstd-compressed tar of `files`, or nothing if there are none.
///
/// # Errors
///
/// Returns an error if building or compressing the archive fails.
pub fn archive_data_files(files: &[(String, Vec<u8>)]) -> std::io::Result<Vec<u8>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let mut tar_builder = tar::Builder::new(Vec::new());
    for (rel_path, contents) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_builder.append_data(&mut header, rel_path, contents.as_slice())?;
    }
    let tar_bytes = tar_builder.into_inner()?;
    zstd::encode_all(tar_bytes.as_slice(), 3)
}

/// Extract a data files archive into `dir`. An empty archive extracts
/// nothing.
///
/// # Errors
///
/// Returns an error if the archive is invalid, holds an absolute or
/// parent-traversal path, or writing fails.
pub fn extract_data_files(archive: &[u8], dir: &Path) -> std::io::Result<()> {
    if archive.is_empty() {
        return Ok(());
    }
    let tar_bytes = zstd::decode_all(archive)?;
    let mut archive = tar::Archive::new(tar_bytes.as_slice());

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        if path.is_absolute()
            || path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsafe path in data files archive: {}", path.display()),
            ));
        }

        let out_path = dir.join(&path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut outfile = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut outfile)?;
    }
    Ok(())
}

/// Read a little-endian `u32`.
///
/// # Errors
///
/// Returns [`std::io::ErrorKind::InvalidData`] unless `buf` is 4 bytes.
pub fn read_le_u32(buf: &[u8]) -> std::io::Result<u32> {
    let arr: [u8; 4] = buf.try_into().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "not enough bytes for u32")
    })?;
    Ok(u32::from_le_bytes(arr))
}

/// Read a little-endian `u64`.
///
/// # Errors
///
/// Returns [`std::io::ErrorKind::InvalidData`] unless `buf` is 8 bytes.
pub fn read_le_u64(buf: &[u8]) -> std::io::Result<u64> {
    let arr: [u8; 8] = buf.try_into().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "not enough bytes for u64")
    })?;
    Ok(u64::from_le_bytes(arr))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tz_data_files() -> Vec<(String, Vec<u8>)> {
        vec![
            ("tzdata/zoneinfo/UTC".to_string(), b"TZif data".to_vec()),
            (
                "tzdata/zoneinfo/Asia/Tokyo".to_string(),
                b"TZif tokyo data".to_vec(),
            ),
        ]
    }

    fn build_v4(
        precompiled: &[u8],
        data_files: &[(String, Vec<u8>)],
        build_inputs: &[u8],
    ) -> Vec<u8> {
        let artifact = crate::PrecompiledArtifact::new(precompiled.to_vec());
        let mut bytes = Vec::new();
        write(&artifact, data_files, build_inputs, &mut bytes).unwrap();
        bytes
    }

    /// Build a v3 file, as pyeryx wrote before build inputs were stored.
    fn build_v3(precompiled: &[u8], data_files: &[(String, Vec<u8>)]) -> Vec<u8> {
        let data_files = archive_data_files(data_files).unwrap();
        let mut bytes = precompiled.to_vec();
        bytes.extend_from_slice(&data_files);
        bytes.extend_from_slice(&(precompiled.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(data_files.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&[7u8; 32]);
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(V3_FOOTER_MAGIC);
        bytes
    }

    fn inputs() -> BuildInputs {
        BuildInputs::new()
            .with_imports(["mypkg".to_string(), "json".to_string()])
            .with_setup_code(Some("ready = True\n".to_string()))
            .with_callbacks(vec![CallbackDeclaration {
                name: "get_time".to_string(),
                description: "Returns the time".to_string(),
                parameters_schema_json: r#"{"type":"object"}"#.to_string(),
            }])
            .with_target(Some("x86_64-unknown-linux-gnu".to_string()))
            .with_cpu_features(Some(CpuFeatureLevel::X86_64v3))
    }

    fn mapped(bytes: &[u8]) -> (u64, u64, u64, Option<[u8; 32]>) {
        match read_layout(&mut Cursor::new(bytes)).unwrap() {
            Layout::Mapped {
                component_len,
                data_files_len,
                build_inputs_len,
                component_hash,
            } => (
                component_len,
                data_files_len,
                build_inputs_len,
                component_hash,
            ),
            Layout::V2 => panic!("expected a mapped layout"),
        }
    }

    #[test]
    fn v1_is_mapped_whole() {
        let raw = b"\x7fELF some precompiled bytes";
        assert_eq!(mapped(raw), (raw.len() as u64, 0, 0, None));
    }

    #[test]
    fn v4_component_starts_at_offset_zero() {
        let precompiled = b"precompiled wasm component";
        let bytes = build_v4(precompiled, &[], &[]);

        assert_eq!(&bytes[..precompiled.len()], precompiled);
        assert_eq!(&bytes[bytes.len() - 8..], FOOTER_MAGIC);
        let hash = crate::PrecompiledArtifact::new(precompiled.to_vec())
            .content_hash()
            .unwrap();
        assert_eq!(mapped(&bytes), (precompiled.len() as u64, 0, 0, Some(hash)));
    }

    #[test]
    fn v4_sections_hold_the_data_files_and_build_inputs() {
        let precompiled = b"precompiled wasm component";
        let bytes = build_v4(precompiled, &tz_data_files(), b"build inputs");

        let layout = read_layout(&mut Cursor::new(&bytes)).unwrap();
        let (data_files, build_inputs) = layout.sections().unwrap();
        let range =
            |r: Range<u64>| usize::try_from(r.start).unwrap()..usize::try_from(r.end).unwrap();
        assert_eq!(&bytes[range(build_inputs)], b"build inputs");

        let dir = tempfile::tempdir().unwrap();
        extract_data_files(&bytes[range(data_files)], dir.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("tzdata/zoneinfo/Asia/Tokyo")).unwrap(),
            b"TZif tokyo data"
        );
    }

    #[test]
    fn v4_rejects_footer_lengths_that_do_not_match() {
        let mut bytes = build_v4(b"precompiled", &tz_data_files(), b"inputs");
        // Dropping a leading byte keeps the footer but breaks its lengths.
        bytes.remove(0);
        assert!(read_layout(&mut Cursor::new(&bytes)).is_err());
    }

    #[test]
    fn v4_rejects_overflowing_footer_lengths() {
        let mut bytes = build_v4(b"precompiled", &[], &[]);
        let footer = bytes.len() - usize::try_from(FOOTER_LEN).unwrap();
        bytes[footer..footer + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(read_layout(&mut Cursor::new(&bytes)).is_err());
    }

    #[test]
    fn v4_rejects_bad_version() {
        let mut bytes = build_v4(b"precompiled", &[], &[]);
        let version = bytes.len() - 12;
        bytes[version..version + 4].copy_from_slice(&99u32.to_le_bytes());
        assert!(read_layout(&mut Cursor::new(&bytes)).is_err());
    }

    #[test]
    fn v3_is_still_read() {
        let precompiled = b"precompiled wasm component";
        let bytes = build_v3(precompiled, &tz_data_files());

        let (component_len, data_files_len, build_inputs_len, hash) = mapped(&bytes);
        assert_eq!(component_len, precompiled.len() as u64);
        assert!(data_files_len > 0);
        assert_eq!(build_inputs_len, 0);
        assert_eq!(hash, Some([7u8; 32]));
    }

    #[test]
    fn v3_rejects_bad_version() {
        let mut bytes = build_v3(b"precompiled", &[]);
        let version = bytes.len() - 12;
        bytes[version..version + 4].copy_from_slice(&4u32.to_le_bytes());
        assert!(read_layout(&mut Cursor::new(&bytes)).is_err());
    }

    #[test]
    fn v2_is_detected() {
        let mut bytes = V2_MAGIC.to_vec();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(read_layout(&mut Cursor::new(&bytes)).unwrap(), Layout::V2);
    }

    #[test]
    fn build_inputs_round_trip() {
        let json = inputs().to_json().unwrap();
        assert_eq!(BuildInputs::from_json(&json).unwrap(), inputs());

        let json = BuildInputs::new().to_json().unwrap();
        assert_eq!(BuildInputs::from_json(&json).unwrap(), BuildInputs::new());
    }

    #[test]
    fn build_inputs_reject_malformed_json() {
        for json in [
            &b"not json"[..],
            br#"{"imports": [1], "callbacks": []}"#,
            br#"{"imports": [], "callbacks": [{}]}"#,
            br#"{"callbacks": []}"#,
            br#"{"imports": [], "callbacks": [], "cpu_features": "x86-64-v9"}"#,
        ] {
            assert!(BuildInputs::from_json(json).is_err(), "{json:?}");
        }
    }

    #[test]
    fn collect_data_files_keeps_py_and_skips_pyc() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(base.join("pkg")).unwrap();
        std::fs::write(base.join("pkg/__init__.py"), "# python").unwrap();
        std::fs::write(base.join("pkg/module.pyc"), "bytecode").unwrap();
        std::fs::write(base.join("pkg/data.bin"), "binary data").unwrap();

        let files = collect_data_files(base).unwrap();
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"pkg/__init__.py"));
        assert!(!names.contains(&"pkg/module.pyc"));
        assert!(names.contains(&"pkg/data.bin"));
    }

    #[test]
    fn collect_data_files_nonexistent_dir() {
        assert!(
            collect_data_files(Path::new("/nonexistent/path"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn extract_rejects_parent_traversal() {
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        // `append_data` refuses `..`, so write the path into the header.
        header.as_gnu_mut().unwrap().name[..7].copy_from_slice(b"../evil");
        header.set_cksum();
        tar_builder.append(&header, &b"x"[..]).unwrap();
        let archive = zstd::encode_all(tar_builder.into_inner().unwrap().as_slice(), 3).unwrap();

        let dir = tempfile::tempdir().unwrap();
        assert!(extract_data_files(&archive, dir.path()).is_err());
    }
}
