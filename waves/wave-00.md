# Wave 0: Environment Provisioning — DoD Checklist

- [x] rustc, cargo installed at latest stable (1.97.1)
- [x] rustfmt, clippy components added
- [ ] cargo-audit, cargo-deny, cargo-llvm-cov, cargo-nextest installed (in progress, background)
- [ ] docker, kubectl, terraform installed (deferred — not available in this environment)
- [x] turboGP cloned and `cargo check` succeeds
- [x] waves/ directory exists with DoD checklist markdown files
- [ ] git pre-commit hook runs cargo fmt --check and cargo clippy
- [ ] CI baseline: security workflow runs cargo-audit + cargo-deny
