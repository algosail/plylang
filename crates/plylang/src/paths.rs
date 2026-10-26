use crate::codes::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    Source,
    Interface,
}

/// `Data/Maybe.ply` and `Data/Maybe.d.ply` both name `Data.Maybe`.
pub fn module_name_from_path(path: &str) -> Result<Vec<String>, Error> {
    let path = path.replace('\\', "/");
    let stem = path
        .strip_suffix(".d.ply")
        .or_else(|| path.strip_suffix(".ply"))
        .ok_or(Error::ExpectedPlyPath)?;
    if stem.is_empty() {
        return Err(Error::EmptyModulePath);
    }
    let mut segments = Vec::new();
    for segment in stem.split('/') {
        if segment.is_empty() {
            return Err(Error::EmptyPathSegment);
        }
        let starts_upper = segment
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase());
        let rest_ok = segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if !starts_upper || !rest_ok {
            return Err(Error::ModuleSegment(segment.to_string()));
        }
        segments.push(segment.to_string());
    }
    Ok(segments)
}

pub fn mode_from_path(path: &str) -> ParseMode {
    if path.ends_with(".d.ply") {
        ParseMode::Interface
    } else {
        ParseMode::Source
    }
}
