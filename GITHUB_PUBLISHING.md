# Publishing BeatCraft Studio on GitHub

This source tree is prepared for `esoexe/beatcraft-studio`. The repository has no remote configured yet. GitHub publication must be completed from an account-controlled machine; do not paste access tokens into chat, source files, or shell history.

## Create and push the repository

Install Git and GitHub CLI, then authenticate securely with `gh auth login`. From the project folder:

```bash
git init -b main
git add .
git commit -m "Release BeatCraft Studio 1.0.0 Rust desktop source"
gh repo create esoexe/beatcraft-studio --public --source=. --remote=origin --push
```

Use `--private` if you prefer a private repository. Alternatively, create an empty repository on GitHub and add it as `origin` before pushing.

## Build platform executables

The GitHub Actions workflow checks formatting, runs Rust checks/tests, and builds the four native executables on Linux, Windows, and macOS. It runs on pull requests, pushes to `main`, manual dispatch, and version tags. The build artifacts are downloadable from the workflow run; they are unsigned executables, not signed installers.

To publish a tagged build after pushing the repository:

```bash
git tag v1.0.0
git push origin v1.0.0
```

Builds are produced on their target operating systems, not cross-compiled. Test output devices, input devices, file dialogs, and packaging on real machines before describing a platform build as production-ready.
