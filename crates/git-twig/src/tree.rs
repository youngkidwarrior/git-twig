use crate::native;
use anyhow::{Context, Result, bail, ensure};
use std::path::{Component, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File { executable: bool },
    Symlink,
    Submodule,
}

pub struct Entry {
    pub path: PathBuf,
    pub kind: Kind,
    pub object_id: String,
}

pub fn parse(bytes: &[u8]) -> Result<Vec<Entry>> {
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "unterminated Git tree"
    );
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let separator = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("missing tree separator")?;
            let header = std::str::from_utf8(&record[..separator])?;
            let mut fields = header.splitn(3, ' ');
            let mode = fields.next().context("missing tree mode")?;
            let object_id = fields.nth(1).context("missing tree object ID")?;
            let kind = match mode {
                "100644" => Kind::File { executable: false },
                "100755" => Kind::File { executable: true },
                "120000" => Kind::Symlink,
                "160000" => Kind::Submodule,
                _ => bail!("unsupported Git mode: {mode}"),
            };
            let path = PathBuf::from(native::os_string(record[separator + 1..].to_vec()));
            ensure!(!path.as_os_str().is_empty() && path.components().all(|part|
            matches!(part, Component::Normal(name) if !name.eq_ignore_ascii_case(".git"))),
            "unsafe tree path: {}", path.display());
            Ok(Entry {
                path,
                kind,
                object_id: object_id.into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_tabs_and_newlines() {
        let entries = parse(b"100644 blob abc\todd\tname\n.txt\0").unwrap();
        assert_eq!(entries[0].path, PathBuf::from("odd\tname\n.txt"));
    }
    #[test]
    fn refuses_unsafe_paths_and_modes() {
        for input in [
            b"100644 blob abc\t../escape\0".as_slice(),
            b"100644 blob abc\t.git/config\0",
            b"100644 blob abc\t/absolute\0",
            b"999999 blob abc\tx\0",
            b"100644 blob abc\tx",
        ] {
            assert!(parse(input).is_err());
        }
    }
}
