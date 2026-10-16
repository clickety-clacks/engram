#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructuredRead {
    pub file: String,
    pub range: [u32; 2],
    pub text: String,
    pub coverage_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructuredEdit {
    pub file: String,
    pub before_text: Option<String>,
    pub after_text: Option<String>,
}

pub(crate) fn bounded_shell_read(
    command: &str,
    stdout: &str,
    workdir: Option<&str>,
    cwd: Option<&str>,
) -> Option<StructuredRead> {
    if stdout.is_empty() {
        return None;
    }
    let words = shell_words(command)?;
    let (path, start, maximum) = match words.as_slice() {
        [program, path] if program == "cat" => (path.as_str(), 1, None),
        [program, dashdash, path] if program == "cat" && dashdash == "--" => {
            (path.as_str(), 1, None)
        }
        [program, n, count, path] if program == "head" && n == "-n" => {
            (path.as_str(), 1, Some(positive(count)?))
        }
        [program, count, path] if program == "head" && count.starts_with('-') => {
            (path.as_str(), 1, Some(positive(&count[1..])?))
        }
        [program, n, count, dashdash, path]
            if program == "head" && n == "-n" && dashdash == "--" =>
        {
            (path.as_str(), 1, Some(positive(count)?))
        }
        [program, count, dashdash, path]
            if program == "head" && count.starts_with('-') && dashdash == "--" =>
        {
            (path.as_str(), 1, Some(positive(&count[1..])?))
        }
        [program, n, count, path] if program == "tail" && n == "-n" && count.starts_with('+') => {
            (path.as_str(), positive(&count[1..])?, None)
        }
        [program, n, count, dashdash, path]
            if program == "tail" && n == "-n" && count.starts_with('+') && dashdash == "--" =>
        {
            (path.as_str(), positive(&count[1..])?, None)
        }
        [program, n, expression, path] if program == "sed" && n == "-n" => {
            let (start, maximum) = sed_expression(expression)?;
            (path.as_str(), start, Some(maximum))
        }
        [program, n, expression, dashdash, path]
            if program == "sed" && n == "-n" && dashdash == "--" =>
        {
            let (start, maximum) = sed_expression(expression)?;
            (path.as_str(), start, Some(maximum))
        }
        _ => return None,
    };

    let lines = stdout.lines().count() as u32;
    if lines == 0 || maximum.is_some_and(|maximum| lines > maximum - start + 1) {
        return None;
    }
    let path_is_absolute = is_absolute_path(path);
    let coverage_complete =
        path_is_absolute || workdir.is_some_and(is_absolute_path) || cwd.is_some();
    Some(StructuredRead {
        file: shell_path(path, workdir, cwd),
        range: [start, start + lines - 1],
        text: stdout.to_string(),
        coverage_complete,
    })
}

fn positive(raw: &str) -> Option<u32> {
    raw.parse::<u32>().ok().filter(|number| *number > 0)
}

fn sed_expression(expression: &str) -> Option<(u32, u32)> {
    let body = expression.strip_suffix('p')?;
    if let Some((start, end)) = body.split_once(',') {
        let start = positive(start)?;
        let end = positive(end)?;
        (end >= start).then_some((start, end))
    } else {
        let line = positive(body)?;
        Some((line, line))
    }
}

fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = command.chars().peekable();
    let mut quote = None;
    let mut started = false;
    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    word.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => word.push(chars.next()?),
                '$' | '`' => return None,
                _ => word.push(ch),
            },
            Some(_) => unreachable!(),
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    started = true;
                }
                '\\' => {
                    word.push(chars.next()?);
                    started = true;
                }
                ' ' | '\t' | '\r' | '\n' => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                '|' | '&' | ';' | '<' | '>' | '$' | '`' | '(' | ')' | '*' | '?' | '[' | ']'
                | '{' | '}' => return None,
                _ => {
                    word.push(ch);
                    started = true;
                }
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    if words.first().is_some_and(|word| {
        word.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
                && !name.starts_with(|ch: char| ch.is_ascii_digit())
        })
    }) {
        return None;
    }
    Some(words)
}

pub(crate) fn is_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with('\\')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn has_windows_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

pub(crate) fn resolve_transcript_path(path: &str, cwd: Option<&str>) -> String {
    shell_path(path, None, cwd)
}

fn shell_path(path: &str, workdir: Option<&str>, cwd: Option<&str>) -> String {
    let joined = if is_absolute_path(path) {
        path.to_owned()
    } else if let Some(workdir) = workdir {
        if is_absolute_path(workdir) {
            join_path(workdir, path)
        } else if let Some(cwd) = cwd {
            join_path(&join_path(cwd, workdir), path)
        } else {
            join_path(workdir, path)
        }
    } else if let Some(cwd) = cwd {
        join_path(cwd, path)
    } else {
        path.to_owned()
    };
    lexical_normalize(&joined)
}

fn join_path(base: &str, tail: &str) -> String {
    if base.is_empty() {
        tail.to_owned()
    } else if base.ends_with('/') || base.ends_with('\\') {
        format!("{base}{tail}")
    } else {
        format!("{base}/{tail}")
    }
}

fn lexical_normalize(path: &str) -> String {
    // Transcript paths are data, so interpret both POSIX and Windows roots
    // independently of the host that imports them. Emit a stable slash form.
    let windows_syntax = cfg!(windows) || has_windows_drive_prefix(path) || path.starts_with('\\');
    let normalized = if windows_syntax {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    };

    let (prefix, rest, rooted) = if has_windows_drive_prefix(&normalized) {
        let prefix = &normalized[..2];
        let rest = &normalized[2..];
        (prefix, rest.trim_start_matches('/'), rest.starts_with('/'))
    } else if normalized.starts_with("//") {
        ("//", normalized.trim_start_matches('/'), true)
    } else if normalized.starts_with('/') {
        ("/", normalized.trim_start_matches('/'), true)
    } else {
        ("", normalized.as_str(), false)
    };

    let mut components = Vec::new();
    for component in rest.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.last().is_some_and(|last| *last != "..") {
                    components.pop();
                } else if !rooted {
                    components.push("..");
                }
            }
            _ => components.push(component),
        }
    }

    let body = components.join("/");
    if prefix == "/" {
        if body.is_empty() {
            "/".to_owned()
        } else {
            format!("/{body}")
        }
    } else if prefix == "//" {
        if body.is_empty() {
            "//".to_owned()
        } else {
            format!("//{body}")
        }
    } else if !prefix.is_empty() && rooted {
        if body.is_empty() {
            format!("{prefix}/")
        } else {
            format!("{prefix}/{body}")
        }
    } else if prefix.is_empty() {
        body
    } else {
        format!("{prefix}{body}")
    }
}

pub(crate) fn parse_patch(patch: &str) -> Vec<StructuredEdit> {
    #[derive(Clone, Copy)]
    enum Kind {
        Add,
        Delete,
        Update,
    }

    struct Block {
        kind: Kind,
        path: String,
        move_to: Option<String>,
        before: String,
        after: String,
    }

    fn flush(block: Option<Block>, edits: &mut Vec<StructuredEdit>) {
        let Some(block) = block else { return };
        if let Some(destination) = block.move_to {
            if block.before.is_empty() || block.after.is_empty() {
                return;
            }
            edits.push(StructuredEdit {
                file: block.path,
                before_text: Some(block.before),
                after_text: None,
            });
            edits.push(StructuredEdit {
                file: destination,
                before_text: None,
                after_text: Some(block.after),
            });
            return;
        }
        let edit = match block.kind {
            Kind::Add if !block.after.is_empty() => StructuredEdit {
                file: block.path,
                before_text: None,
                after_text: Some(block.after),
            },
            Kind::Delete if !block.before.is_empty() => StructuredEdit {
                file: block.path,
                before_text: Some(block.before),
                after_text: None,
            },
            Kind::Update if !block.before.is_empty() && !block.after.is_empty() => StructuredEdit {
                file: block.path,
                before_text: Some(block.before),
                after_text: Some(block.after),
            },
            _ => return,
        };
        edits.push(edit);
    }

    let mut edits = Vec::new();
    let mut current = None::<Block>;
    for line in patch.lines() {
        let header = line
            .strip_prefix("*** Add File: ")
            .map(|path| (Kind::Add, path))
            .or_else(|| {
                line.strip_prefix("*** Delete File: ")
                    .map(|path| (Kind::Delete, path))
            })
            .or_else(|| {
                line.strip_prefix("*** Update File: ")
                    .map(|path| (Kind::Update, path))
            });
        if let Some((kind, path)) = header {
            flush(current.take(), &mut edits);
            let path = path.trim();
            if !path.is_empty() {
                current = Some(Block {
                    kind,
                    path: path.to_string(),
                    move_to: None,
                    before: String::new(),
                    after: String::new(),
                });
            }
            continue;
        }
        let Some(block) = current.as_mut() else {
            continue;
        };
        if let Some(destination) = line.strip_prefix("*** Move to: ") {
            let destination = destination.trim();
            if !destination.is_empty() {
                block.move_to = Some(destination.to_string());
            }
        } else if line.starts_with("@@")
            || line == "*** Begin Patch"
            || line == "*** End Patch"
            || line == "*** End of File"
        {
        } else if let Some(text) = line.strip_prefix('+') {
            block.after.push_str(text);
            block.after.push('\n');
        } else if let Some(text) = line.strip_prefix('-') {
            block.before.push_str(text);
            block.before.push('\n');
        } else if let Some(text) = line.strip_prefix(' ') {
            block.before.push_str(text);
            block.before.push('\n');
            block.after.push_str(text);
            block.after.push('\n');
        }
    }
    flush(current, &mut edits);
    edits
}

pub(crate) fn patch_is_complete(patch: &str) -> bool {
    let mut blocks = Vec::<String>::new();
    let mut current = String::new();
    for line in patch.lines() {
        let is_header = line.starts_with("*** Add File: ")
            || line.starts_with("*** Delete File: ")
            || line.starts_with("*** Update File: ");
        if is_header && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        if is_header || !current.is_empty() {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    !blocks.is_empty() && blocks.iter().all(|block| !parse_patch(block).is_empty())
}

#[cfg(test)]
mod tests {
    use super::{bounded_shell_read, is_absolute_path, parse_patch, shell_path};

    #[test]
    fn rejects_unbounded_shell_syntax() {
        for command in [
            "cat a b",
            "cat a | head",
            "cat a > out",
            "A=1 cat a",
            "A=/tmp cat a",
            "cat *.rs",
            "cat a; cat b",
            "cat (a)",
            "cat $(pwd)",
            "cat `pwd`",
            "ssh host cat a",
            "python -c 'print(1)'",
        ] {
            assert!(bounded_shell_read(command, "x\n", None, None).is_none());
        }
    }

    #[test]
    fn patch_keeps_repeated_paths_and_expands_move() {
        let edits = parse_patch(
            "*** Update File: a\n@@\n-x\n+y\n*** Update File: a\n@@\n-b\n+c\n*** Move to: b\n",
        );
        assert_eq!(edits.len(), 3);
        assert_eq!(edits[0].file, "a");
        assert_eq!(edits[1].file, "a");
        assert_eq!(edits[2].file, "b");
    }

    #[test]
    fn shell_paths_preserve_recorded_posix_and_windows_roots() {
        assert_eq!(
            shell_path("src/a.rs", None, Some("/repo")),
            "/repo/src/a.rs"
        );
        assert_eq!(
            shell_path("src\\a.rs", None, Some(r"C:\repo")),
            "C:/repo/src/a.rs"
        );
        assert_eq!(
            shell_path("../src/a.rs", None, Some(r"C:\repo\pkg")),
            "C:/repo/src/a.rs"
        );
        assert!(is_absolute_path("/repo/src/a.rs"));
        assert!(is_absolute_path(r"C:\repo\src\a.rs"));
        assert!(!is_absolute_path("src/a.rs"));
    }
}
