# VercelBar

A macOS menu-bar app for monitoring Vercel teams and projects. View deployment status, recent deployments, analytics details, open project subpages, or redeploy production with one click.

## Setup

Launch app, add a Vercel API token in **Settings**, select teams and projects you want tot monitor.

## Contributing

Requires Rust and macOS with Xcode command-line tools.

```sh
cargo fmt-check
cargo check --all-targets
cargo lint
```

`cargo build-app` builds the release app, bundles it and moves to `/Applications/VercelBar.app`.

> [!note]
> For development, `VERCEL_TOKEN` can be set in the environment or a local `.env` file.
