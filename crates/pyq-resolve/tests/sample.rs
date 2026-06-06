//! Integration test over the in-repo `examples/sample` project. Exercises the
//! real ty-backed cross-file resolution that the unit-free syntactic extractor
//! cannot do.

use pyq_resolve::{Resolver, TyResolver};

fn sample_root() -> String {
    format!("{}/../../examples/sample", env!("CARGO_MANIFEST_DIR"))
}

fn resolver() -> TyResolver {
    TyResolver::new(&sample_root()).expect("build resolver over examples/sample")
}

#[test]
fn definitions_resolve_to_the_real_class_not_the_import() {
    let defs = resolver().definitions("User").unwrap();
    // The import in app.py is a *reference*, not a definition: exactly one def.
    assert_eq!(defs.len(), 1, "got {defs:?}");
    assert!(defs[0].path.ends_with("models.py"));
}

#[test]
fn references_span_files_via_the_import() {
    let refs = resolver().references("User").unwrap();
    let paths: Vec<&str> = refs.iter().map(|l| l.path.as_str()).collect();
    assert!(paths.iter().any(|p| p.ends_with("app.py")), "got {paths:?}");
    assert!(paths.iter().any(|p| p.ends_with("models.py")), "got {paths:?}");
}

#[test]
fn callers_name_the_enclosing_function() {
    let callers = resolver().callers("User").unwrap();
    // `User(...)` is called inside `main` and inside `make_user`.
    let kinds: Vec<&str> = callers.iter().map(|l| l.kind.as_str()).collect();
    assert!(kinds.contains(&"main"), "got {callers:?}");
    assert!(kinds.contains(&"make_user"), "got {callers:?}");
}
