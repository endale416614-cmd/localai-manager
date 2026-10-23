# LocalAI Manager

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)

A small Rust + Slint launcher for a local AI stack on Windows 10 and 11 (llama.cpp models, ComfyUI, Ollama, Odysseus and more). It starts, stops and watches every app, and it enforces one GPU app at a time.

## Run

`LocalAI-Manager.exe` (or `LocalAI.exe`), with `apps.toml` beside it. Press **Ctrl+K** for the command palette.

## One-click install

Build `LocalAI-Setup.exe` (see [Build](#build)) and double-click it. It needs no answers:

1. It picks the folder: `--dir <folder>` if given, else the folder the desktop `LocalAI.lnk` points into when that shortcut starts a `localai*.exe` with `apps.toml` beside it (an update keeps `apps.toml` and the models), else `LocalAI` on the fixed drive with the most free space.
2. It writes the manager exe there, first as `<name>.new` and only then in place of the old one, so a failed write leaves the old version working. On an update the previous exe is kept as `<name>.old`; if the old version is still running, it says so and you restart LocalAI yourself.
3. It puts a LocalAI shortcut on the desktop and in the Start menu, and warns if the WebView2 Runtime is missing. With a warning, or with the old version still running, the window waits for Enter.
4. It starts the manager.

On a new install the manager has no `apps.toml` yet, so it opens a console and:

1. downloads llama.cpp b11719 (CUDA 12.4 with an NVIDIA driver, Vulkan otherwise);
2. downloads Qwen3.6-35B-A3B UD-Q4_K_XL (22.4 GB), unless chat models of 1 GiB or more are already on the PC (see below): then it lists them in `apps.toml` instead;
3. checks each file's size and SHA-256 (pinned in `src/setup.rs`), and resumes interrupted downloads on the next run;
4. writes `start-qwen36.bat` and `apps.toml`, and puts a LocalAI shortcut on the desktop;
5. opens the manager.

`LocalAI.exe --setup` re-runs it in an existing folder: files already present are verified rather than downloaded again, and an existing `apps.toml` gets the Qwen3.6 entry added at the top.

## Models found on the PC

At every start, a background scan (`src/detect.rs`) adds what `apps.toml` does not list yet, under an `# --- Auto-detected` comment. It only runs for the `apps.toml` beside the exe, so a dev run from the repo never appends to the shipped one. The file is saved through `apps.toml.tmp` and the previous text stays as `apps.toml.bak`; a file the scan cannot read (saved as UTF-16, say) is reported and left alone.

- **GGUF chat models** in `models\` beside the exe, LM Studio's folders, the Hugging Face cache, and `X:\AI`, `X:\models`, `X:\LLM`, `X:\LocalAI\models` on every fixed drive. Each one gets a GPU tile that runs `llama-server` from `llama.cpp\` (or PATH) on the next free port from 8090, with 32K context. Only the GGUF header before the tokenizer is read, which keeps the scan fast and a damaged file harmless. Adapters, vision projectors and vocab files (`general.type` other than `model`), embedding models (a `*.pooling_type` key), and image/video models and encoders (by architecture) are skipped.
- Known models (Qwen3.6, Qwen3.8, TranslateGemma, Typhoon and their abliterated builds) get display names and flags: Qwen3.6 samplers as in `start-qwen36.bat`, thinking off for the abliterated translation models.
- **Ollama** (`%LOCALAPPDATA%\Programs\Ollama` or PATH) as a service tile listing its pulled models.
- **Odysseus**: an `odysseus` folder with `app.py` and `venv`, at a drive root, one folder below it, or in the user folder. Its venv Python runs at the next start, so the folder must be owned by you, Administrators or SYSTEM. It is not added when an app in `apps.toml` already has that folder as `cwd` or uses port 7000.

A model counts as listed when its file name appears in `apps.toml` or in a `.bat` an app runs (a relative `.bat` path is read from the app's `cwd`). To hide one, comment out its lines: the file name stays in the file, so it is not added again.

## Odysseus

- **Models**: after the scan, and again the first time Odysseus is ready, every chat app and Ollama is added to Odysseus as a model endpoint (`http://127.0.0.1:<port>/v1`). `src/odysseus.rs` runs a short script with Odysseus' own venv Python and plain `sqlite3`. It never imports Odysseus' code (that would run its database set-up next to the live server) and never creates a database: it only adds a row to the existing `model_endpoints` table for each URL that is not there yet (`localhost` and `127.0.0.1` count as one, so does a trailing `/`). The database is `DATABASE_URL` (`sqlite:///<path>` only), else `app.db` in `ODYSSEUS_DATA_DIR` (default `data\`). If there is no database yet, the log says to start Odysseus once; the models are linked when it is first ready. The result is shown in Odysseus' log. An endpoint deleted in Odysseus comes back on the next start.
- **Window**: **Open** shows Odysseus inside the manager (WebView2), under a bar with **← Back to LocalAI** and **Reload**. Back hides it and keeps the page, so the login and open chats survive. Logins are stored in `%LOCALAPPDATA%\LocalAI\WebView2`. Without the WebView2 Runtime it opens in the browser. Only the Odysseus address stays inside: any other link or pop-up opens in the default browser, and only `http` and `https` links are passed on. The view runs without GPU acceleration so that the models keep the VRAM, and it goes to low-memory mode while hidden.

## Chat

The **Chat** tab talks to the first running app with `chat = true`, or else to the first one in `apps.toml`. **Start** loads the model, and stops whichever GPU app is in the way (the button names it). Replies stream from llama-server's `/v1/chat/completions`. While Qwen3.6 thinks, the tail of its reasoning is shown, and it collapses to a word count once the answer starts. Chats are not saved.

## Build

```bash
cargo build --release
```

The installer embeds that exe, so build it second:

```bash
cargo build --release --manifest-path installer/Cargo.toml
```

Then copy `installer\target\release\localai-setup.exe` to `dist\LocalAI-Setup.exe`.

`cargo test` runs:

- config validation;
- the GPU guard;
- the HTTP status parser;
- the chat stream parser and Stop;
- the setup's apps.toml merge and SHA-256;
- the model scan: GGUF header reading (control characters, damaged headers, adapters, embedding models), and the apps.toml merge (added once, hidden when commented out, Odysseus listed already, a file without a final newline, an unreadable file left untouched);
- `apps.toml` saving with its `.bak`;
- the Odysseus script against a temporary SQLite database (skipped when no Python is on PATH);
- which addresses stay inside the Odysseus view;
- a test that a job-object kill takes down grandchild processes.

`cargo test` in `installer\` checks the `.new` file name and which shortcut targets count as an install.

## How it works

- **`apps.toml`** lists each app: command, port, health check, and group (`gpu` / `svc` / `app`). Launch flags stay in the `.bat` files.
- **`job.rs`**: each gpu/svc app runs inside its own Windows job object with kill-on-close.
  - The app is spawned suspended, assigned to the job, then resumed, so no child process can escape the job.
  - Stop or Quit kills the whole process tree.
- **`supervisor.rs`**:
  - pipes stdout/stderr into a 400-line log;
  - polls health (`GET <path>` must return 200, or `tcp` = the port accepts connections) to move an app from Starting to Ready;
  - refuses to start an app whose port is already in use.
- **`guard.rs`**: starting a second GPU app asks to switch. The manager stops the running one first and starts the new one once VRAM is free.
- **`metrics.rs`**: VRAM (summed over every NVIDIA card) and GPU load through NVML, RAM and swap through sysinfo, sampled once a second on a background thread.
- **`detect.rs`**, **`odysseus.rs`**, **`web.rs`**: the model scan, the Odysseus link and the embedded Odysseus view described above.
- Desktop/terminal apps (`group = "app"`) are launch-only and keep running after the manager quits.

## Startup

The software renderer was chosen on purpose. On this machine the first paint takes about 85 ms, against about 330 ms with the OpenGL (femtovg) renderer. The figures were measured externally, from process start to the first painted frame.

`LocalAI-Manager.exe --bench-startup` prints in-process timings to stdout and exits.

## License

MIT, see [LICENSE](LICENSE). The user interface is built with [Slint](https://slint.dev), used under the Slint Royalty-free License 2.0 (attribution badge at the top of this page). Other dependencies are MIT or Apache-2.0 licensed.
