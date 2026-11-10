# openSUSE Build Service

The GitHub release workflow can submit source packages to OBS. OBS builds RPM
and Debian packages for the repositories enabled on the OBS project and
publishes them according to that project's publishing configuration. GitHub
continues to publish the existing binary, `.deb`, and `cargo-generate-rpm`
`.rpm` assets.

The source archives include the Svelte build output and vendored Rust crates,
so both OBS builds can run offline. `packaging/rpm/controldeck.spec` and
`debian/` provide the OBS packaging metadata; the package metadata in
`Cargo.toml` remains in place for the existing GitHub release.

## Setup

1. Create the `controldeck` package in the desired OBS project. The default is
   `home:nomachine`; override it with the GitHub repository variable
   `OBS_PROJECT`.
2. Configure the project's build repositories and publishing targets in OBS.
3. Add the GitHub repository variable `OBS_PUBLISH=true` to enable submissions.
   The workflow does not contact OBS when this variable is unset or false.
4. Add repository secrets `OBS_USER` and `OBS_PASSWORD` for an OBS account
   permitted to commit to that package.
5. Optionally set `OBS_API_URL` if using a non-default OBS instance. It defaults
   to `https://api.opensuse.org`.

Each release updates the OBS package sources with a versioned archive and spec.
The commit triggers an OBS build; OBS publishes successful packages according
to the project configuration.
