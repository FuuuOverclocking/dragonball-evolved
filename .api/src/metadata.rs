//! What an operation carries besides its types, all of it static and checked
//! while compiling: names, docs and the `config`, `route` and `cli`
//! declarations.

/// One operation's HTTP binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// The method as declared, e.g. `PUT`.
    pub method: &'static str,
    /// The path rebuilt from its tokens without spacing artifacts, e.g.
    /// `/v1/block-devices`.
    pub path: &'static str,
}

/// One operation, as seen from outside Rust's type system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    /// The operation's name, stable across builds.
    pub name: &'static str,
    /// Its doc comment: one line per `///`, joined by newlines, empty if it has
    /// none.
    pub doc: &'static str,
    /// The config prefix it binds, exactly as declared including a `[]`
    /// suffix; `None` when config files do not carry it.
    pub config: Option<&'static str>,
    /// Its HTTP binding; `None` when it is not reachable over HTTP.
    pub route: Option<Route>,
    /// The module customizing its CLI, recorded but never resolved here.
    pub cli: Option<&'static str>,
}

pub(crate) const fn checked_config(path: &'static str) -> Option<&'static str> {
    assert_valid_config_path(path);
    Some(path)
}

pub(crate) const fn checked_route(method: &'static str, path: &'static str) -> Option<Route> {
    assert_valid_route(method, path);
    Some(Route { method, path })
}

pub(crate) const fn duplicate_config() -> Option<&'static str> {
    panic!("an operation declares `config` at most once")
}

pub(crate) const fn duplicate_route() -> Option<Route> {
    panic!("an operation declares `route` at most once")
}

pub(crate) const fn duplicate_cli() -> Option<&'static str> {
    panic!("an operation declares `cli` at most once")
}

/// A config path is a static dot-separated binding: it starts with `.`, its
/// segments are nonempty word characters, and a single `[]` may suffix the
/// very end to declare an array. Placeholders, wildcards and index
/// expressions never bind.
pub(crate) const fn assert_valid_config_path(path: &str) {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes[0] != b'.' {
        panic!("a config path starts with `.`");
    }
    let mut i = 1;
    let mut segment = 0;
    let mut array = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'[' {
            if segment == 0 || array || i + 2 != bytes.len() || bytes[i + 1] != b']' {
                panic!("`[]` may occur only once, as the suffix of the final segment");
            }
            array = true;
            i += 2;
        } else if byte == b'.' {
            if segment == 0 {
                panic!("config path segments are not empty");
            }
            segment = 0;
            i += 1;
        } else if is_word_byte(byte) {
            segment += 1;
            i += 1;
        } else {
            panic!("config path segments hold letters, digits and `_` only");
        }
    }
    if segment == 0 && !array {
        panic!("config path segments are not empty");
    }
}

/// A route is static on both sides: the method is uppercase ASCII, and the
/// path is `/` or `/`-separated nonempty segments of word characters and
/// `-`, with no trailing slash. Placeholders, wildcards and query fragments
/// fall outside that charset.
pub(crate) const fn assert_valid_route(method: &str, path: &str) {
    let method = method.as_bytes();
    if method.is_empty() {
        panic!("a route method is not empty");
    }
    let mut i = 0;
    while i < method.len() {
        if !matches!(method[i], b'A'..=b'Z') {
            panic!("a route method is uppercase ASCII, like PUT");
        }
        i += 1;
    }

    let path = path.as_bytes();
    if path.is_empty() || path[0] != b'/' {
        panic!("a route path starts with `/`");
    }
    i = 1;
    let mut segment = 0;
    while i < path.len() {
        let byte = path[i];
        if byte == b'/' {
            if segment == 0 {
                panic!("route path segments are not empty");
            }
            segment = 0;
        } else if is_word_byte(byte) || byte == b'-' {
            segment += 1;
        } else {
            panic!("route path segments hold letters, digits, `_` and `-` only");
        }
        i += 1;
    }
    if path.len() > 1 && segment == 0 {
        panic!("a route path does not end with `/`");
    }
}

const fn is_word_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}
