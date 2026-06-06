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
fn captures_setdefault_membership_and_aliased_env_access() {
    let src = r#"
import os
from os import environ, getenv
import os as o

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "app.settings")
if "DJANGO_SUPERUSER_PASSWORD" not in os.environ:
    pass
if "EAGER" in os.environ:
    pass
a = environ.get("FROM_ENVIRON_ALIAS")
b = environ["SUBSCRIPT_ALIAS"]
c = getenv("BARE_GETENV")
d = o.getenv("OS_ALIAS_GETENV")
whole = os.environ
"#;
    let idx = extract("entry.py", src);
    let env: Vec<&str> = idx
        .inputs
        .iter()
        .filter(|i| i.kind == InputKind::Env)
        .map(|i| i.value.as_str())
        .collect();

    // setdefault is a read-with-fallback
    assert!(env.contains(&"DJANGO_SETTINGS_MODULE"), "{env:?}");
    // membership tests, both `in` and `not in`
    assert!(env.contains(&"DJANGO_SUPERUSER_PASSWORD"), "{env:?}");
    assert!(env.contains(&"EAGER"), "{env:?}");
    // aliased access via `from os import environ` and bare/aliased getenv
    assert!(env.contains(&"FROM_ENVIRON_ALIAS"), "{env:?}");
    assert!(env.contains(&"SUBSCRIPT_ALIAS"), "{env:?}");
    assert!(env.contains(&"BARE_GETENV"), "{env:?}");
    assert!(env.contains(&"OS_ALIAS_GETENV"), "{env:?}");
    // whole-dict bind exposes unknown keys → flagged dynamic
    assert!(env.contains(&"<dynamic>"), "{env:?}");
}

#[test]
fn captures_cli_args_and_settings_fields() {
    let src = r#"
import argparse, click
from pydantic_settings import BaseSettings

class Settings(BaseSettings):
    db_url: str
    port: int = 5432
    debug = False

p = argparse.ArgumentParser()
p.add_argument("--verbose")

@click.option("--count")
def run(count):
    pass
"#;
    let idx = extract("cli.py", src);

    let args: Vec<&str> = idx
        .inputs
        .iter()
        .filter(|i| i.kind == InputKind::Arg)
        .map(|i| i.value.as_str())
        .collect();
    assert!(args.contains(&"--verbose"));
    assert!(args.contains(&"--count"));

    let settings: Vec<&str> = idx
        .inputs
        .iter()
        .filter(|i| i.kind == InputKind::Setting)
        .map(|i| i.value.as_str())
        .collect();
    assert_eq!(settings, vec!["db_url", "port"]); // `debug` is unannotated
}

#[test]
fn captures_import_edges_with_module_level_and_names() {
    let src = r#"
import os
import os.path
from pkg.models import User, make_user
from . import sibling
from ..pkg import thing
"#;
    let idx = extract("pkg/app.py", src);
    let by_module = |m: &str| idx.imports.iter().find(|i| i.module == m).unwrap();

    assert_eq!(by_module("os").level, 0);
    assert_eq!(by_module("os.path").level, 0);

    let models = by_module("pkg.models");
    assert_eq!(models.level, 0);
    assert_eq!(models.names, vec!["User", "make_user"]);

    // `from . import sibling` — empty module, level 1, the name carried through.
    let dot = idx.imports.iter().find(|i| i.module.is_empty()).unwrap();
    assert_eq!(dot.level, 1);
    assert_eq!(dot.names, vec!["sibling"]);

    // `from ..pkg import thing` — module `pkg`, level 2.
    let up = by_module("pkg");
    assert_eq!(up.level, 2);
}

#[test]
fn parse_errors_are_non_fatal() {
    // A half-written file an agent is mid-edit on still answers.
    let idx = extract("broken.py", "def f(:\n    User(");
    // Should not panic; may extract little, but must produce a FileIndex.
    assert_eq!(idx.path, "broken.py");
}

#[test]
fn recovers_facts_before_a_trailing_syntax_error() {
    // The "half-edited file still answers" guarantee: statements that parsed
    // before the error must still be indexed, not zeroed out by the error.
    let src = r#"
import os

def alpha():
    return 1

KEY = os.environ["EARLY_KEY"]

class Broken(
"#;
    let idx = extract("wip.py", src);

    assert!(
        idx.defs.iter().any(|d| d.name == "alpha"),
        "def before the error should survive: {:?}",
        idx.defs
    );
    assert!(
        idx.inputs.iter().any(|i| i.value == "EARLY_KEY"),
        "env read before the error should survive: {:?}",
        idx.inputs
    );
}
