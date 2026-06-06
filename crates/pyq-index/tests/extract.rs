use pyq_index::{extract, DefKind, InputKind};

const SRC: &str = r#"
from pkg.models import User, make_user

GREETING = "hi"

def main():
    u = make_user("ada")
    admin = User("root")
    helper = inner
    def inner():
        return User
    print(u, admin, helper)
"#;

#[test]
fn captures_defs_with_kinds_and_nesting() {
    let idx = extract("app.py", SRC);

    let find = |name: &str| idx.defs.iter().find(|d| d.name == name).unwrap();

    assert_eq!(find("User").kind, DefKind::Import);
    assert_eq!(find("make_user").kind, DefKind::Import);
    assert_eq!(find("GREETING").kind, DefKind::Variable);
    assert_eq!(find("main").kind, DefKind::Function);

    // `inner` is defined inside `main`.
    let inner = find("inner");
    assert_eq!(inner.kind, DefKind::Function);
    assert!(inner.nested);
    assert!(!find("main").nested);
}

#[test]
fn distinguishes_calls_from_plain_refs() {
    let idx = extract("app.py", SRC);

    let calls: Vec<_> = idx.refs.iter().filter(|r| r.is_call).collect();
    assert!(calls.iter().any(|r| r.name == "make_user"));
    assert!(calls.iter().any(|r| r.name == "User")); // User("root")
    assert!(calls.iter().any(|r| r.name == "print"));

    // `inner` used as a value (not called) and `User` returned bare are refs.
    let bare: Vec<_> = idx.refs.iter().filter(|r| !r.is_call).collect();
    assert!(bare.iter().any(|r| r.name == "inner"));
    assert!(bare.iter().any(|r| r.name == "User")); // `return User`
}

#[test]
fn captures_env_and_file_inputs_and_buckets_dynamic() {
    let src = r#"
import os
a = os.getenv("DEBUG")
b = os.environ["DATABASE_URL"]
c = os.environ.get("TIMEOUT", "30")
d = os.getenv(some_var)
open("settings.ini")
"#;
    let idx = extract("config.py", src);
    let env: Vec<&str> = idx
        .inputs
        .iter()
        .filter(|i| i.kind == InputKind::Env)
        .map(|i| i.value.as_str())
        .collect();
    assert!(env.contains(&"DEBUG"));
    assert!(env.contains(&"DATABASE_URL"));
    assert!(env.contains(&"TIMEOUT"));
    assert!(env.contains(&"<dynamic>")); // computed key, not guessed

    let files: Vec<&str> = idx
        .inputs
        .iter()
        .filter(|i| i.kind == InputKind::File)
        .map(|i| i.value.as_str())
        .collect();
    assert_eq!(files, vec!["settings.ini"]);
}

#[test]
fn parse_errors_are_non_fatal() {
    // A half-written file an agent is mid-edit on still answers.
    let idx = extract("broken.py", "def f(:\n    User(");
    // Should not panic; may extract little, but must produce a FileIndex.
    assert_eq!(idx.path, "broken.py");
}
