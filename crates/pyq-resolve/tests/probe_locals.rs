//! Probe: does ty resolve a *function-local* variable's references when anchored
//! at the local's definition offset? `workspace_symbols` never surfaces locals,
//! which is why ty appears blind to them — but `find_references` takes a raw
//! offset, so a precise anchor should resolve the local, scope and all. If this
//! passes, the syntactic scan can be demoted to a pure locator and every result
//! becomes ty-precise (no approximation, nothing to disclose).

use pyq_resolve::TyResolver;
use ruff_text_size::TextSize;
use std::fs;

#[test]
fn ty_resolves_function_local_from_its_offset() {
    let dir = std::env::temp_dir().join(format!("pyq_probe_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let src = "def f():\n    tally = 0\n    tally = tally + 1\n    return tally\n";
    fs::write(dir.join("m.py"), src).unwrap();

    let resolver = TyResolver::new(dir.to_str().unwrap(), Default::default())
        .expect("build resolver");

    // Anchor at the local's definition (the first `tally`).
    let offset = TextSize::try_from(src.find("tally").unwrap()).unwrap();
    let refs = resolver.references_at("m.py", offset);

    fs::remove_dir_all(&dir).ok();

    // If ty resolves the local, we get its multiple uses (write + reads), all in
    // m.py. A result of 0 would mean ty genuinely can't resolve locals from an
    // offset (and the locate-then-resolve plan would need rethinking).
    assert!(
        refs.len() >= 2,
        "ty should resolve the function-local `tally` from its offset; got {refs:?}"
    );
    assert!(refs.iter().all(|l| l.path == "m.py"), "{refs:?}");
}
