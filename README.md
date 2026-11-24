# lamasery

A home for your llamas: pick a llama.cpp model from a menu, run it, watch its
colored output, and point the llama.cpp web UI, Claude Code or any other tool
at it.

```
cargo build --release
./target/release/lamasery -c lamasery.yaml      # or put lamasery.yaml next to the binary
./target/release/lamasery -c lamasery.yaml qwen # skip the menu, start a matching model
./target/release/lamasery -c lamasery.yaml check  # validate, print each llama-server command
./target/release/lamasery init                  # write an example config next to the binary
```

Without `--config`, lamasery reads `lamasery.yaml` next to its own binary.

## While a model runs

| key | |
|---|---|
| `o` | open the llama.cpp web UI in your browser |
| `t` | launch a tool (Claude Code, a shell with OpenAI env vars, ...) against the model |
| `r` | restart (or start again after it exits) |
| `l` | pause / resume log output |
| `c` | clear the screen |
| `q`, `ctrl-c` | stop the server and go back to the menu (`ctrl-c` twice kills) |

Tools take over the terminal while they run; the server keeps running and
you're back in the session when the tool exits.

## Config

See [lamasery.example.yaml](lamasery.example.yaml) for every option. In short:

```yaml
llama_server: ${llama}/build/bin/llama-server
vars:
  llama: ~/Documents/Code/llama.cpp
common_args:          # given to every model
  -fa: on
models:
  - name: Qwen3.6 35B
    model: ${llama}/Qwen3.6-35B-A3B-MTP-UD-Q4_K_XL.gguf
    port: 8080
    alias: qwen3.6      # the model id tools ask for
    args:
      -c: 131072
      --no-warmup: true
      --chat-template-kwargs: '{"preserve_thinking": true}'
tools:
  - name: Claude Code
    command: claude
    env:
      ANTHROPIC_BASE_URL: ${base_url}
      ANTHROPIC_MODEL: ${model}
```

`args` can also be a list of shell-style strings (`- -c 131072`). Each entry is
split into words like a shell would, so quote anything that must stay one word.
