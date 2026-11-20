<h1 align="center">
  <img width="64" src="https://github.com/user-attachments/assets/647f53e3-3332-43a8-ad27-71dbad1a9694" />
  <br/>
  VercelBar
</h1>

A macOS menu-bar app for monitoring Vercel teams and projects. View deployment status, recent deployments, analytics details, open project subpages, or redeploy production with one click.

<p align="center"><img width="636" height="512" alt="Screenshot 2026-11-20 at 13 00 33" src="https://github.com/user-attachments/assets/78035261-463c-4d24-9c5f-a7b6509e65d7" /></p>

### Setup

Launch app, add a Vercel API token in **Settings**, select teams and projects you want to monitor.

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
