# README animation

`workflow.gif` is a 15-second, looping illustration of LeafTerm's workflow. It uses a fictional **Nebula** project and invented code and CLI output. It is not a screen recording and contains no user project data or conversation history.

`workflow-poster.png` is a still alternative. Both are generated from vector-style drawing commands in [`scripts/render-readme-demo.py`](../../scripts/render-readme-demo.py); no external artwork or screenshots are used.

To regenerate, install Pillow in your Python environment, then run from the repository root:

```sh
python3 scripts/render-readme-demo.py
```

The script uses Arial/Menlo on macOS, with DejaVu font fallbacks on Linux. These documentation assets do not add a runtime dependency to LeafTerm.
