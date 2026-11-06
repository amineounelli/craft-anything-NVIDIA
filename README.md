# Craft Anything — NVIDIA NIM fork

Simple, personal access to the NVIDIA NIM API: no Claude Code, no agent routing, no other providers. Just your NVIDIA API key, a NVIDIA model, and the Craft Anything engine.

## What this is

A NVIDIA-only fork of Craft Anything. All non-NVIDIA backends (Anthropic, OpenAI, OpenRouter, Gemini, Codex, agent CLIs) are removed. The app, CLI and MCP server talk to `https://integrate.api.nvidia.com/v1` only.

## Kept models

| Model | Best for |
|-------|----------|
| `nvidia/nemotron-4-340b-instruct` | General reasoning and coding |
| `nvidia/llama-3.1-nemotron-70b-instruct` | Code generation |
| `nvidia/llama-3.1-nemotron-ultra-253b-v1` | Complex multi-step reasoning |
| `nvidia/nemotron-3-super-120b-a12b` | Efficient code analysis |
| `deepseek-ai/deepseek-coder-6.7b-instruct` | Specialist coding |
| `nvidia/nemotron-3.5-super-vl-120b-a12b` | Vision-language |

## Get started

1. Get an API key from [build.nvidia.com](https://build.nvidia.com).
2. Set it: `export NVIDIA_API_KEY="your-key"` (or add it in Settings → AI).
3. Run: `cargo run --release -p craft-anything` (desktop app) or `cargo install --path apps/craft-anything-cli` (CLI).
4. Add your key in Settings → AI, pick a model, build.

The NVIDIA API key is reusable — it is not single-use. If a model returns `404 ... Not found for account`, that model is not enabled in your NVIDIA account; enable it on build.nvidia.com or pick another kept model.

Pure Rust, MIT OR Apache-2.0.
