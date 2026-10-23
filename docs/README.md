# Gumka developer documentation

How Gumka is built and why. For how to work in the repository (commands, rules, conventions),
start with [AGENTS.md](../AGENTS.md). The user-facing introduction is the top-level
[README](../README.md).

| Document | What it covers |
|---|---|
| [overview.md](overview.md) | What Gumka is, current status, goals and scope, roadmap, references |
| [development.md](development.md) | Setup, running, checks, tests, driving the app, CI |
| [architecture.md](architecture.md) | Crates and modules, threads, platform code, the stack |
| [opening-and-saving.md](opening-and-saving.md) | Host apps, how files arrive, quitting, the save rules |
| [image-io.md](image-io.md) | File formats and metadata: what is kept, dropped or refused |
| [color.md](color.md) | Color-managed display, and the color space the models see |
| [erase-pipeline.md](erase-pipeline.md) | The erase pipeline, step by step |
| [upscale-pipeline.md](upscale-pipeline.md) | The upscale pipeline: tiles, limits, speed |
| [inference.md](inference.md) | The Core ML backend, the compiled-model cache, later backends |
| [models.md](models.md) | The model registry, each model's provenance, inputs and outputs, licenses |
| [ui.md](ui.md) | UI design and the logo |
| [packaging.md](packaging.md) | Packaging, distribution and licensing |

Keep these documents accurate: a change that alters behavior described here updates the document
in the same change.
