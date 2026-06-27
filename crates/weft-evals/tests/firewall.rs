//! Firewall: weft-evals is the deterministic gate. It must NEVER depend on a
//! generator/production crate (weft-author, weft-admission), directly or via
//! dev-dependencies. The dependency arrow is one-way: generators depend on the
//! gate, never the reverse. This is the contamination firewall as a CI failure.

#[test]
fn weft_evals_has_no_generator_or_production_dependencies() {
    let manifest = include_str!("../Cargo.toml");
    for forbidden in ["weft-author", "weft-admission"] {
        assert!(
            !manifest.contains(forbidden),
            "FIREWALL VIOLATION: weft-evals/Cargo.toml references `{forbidden}`. \
             The deterministic gate must not depend on a generator/production crate."
        );
    }
}
