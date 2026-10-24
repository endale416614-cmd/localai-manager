# LocalAI Manager

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)

A small Rust + Slint launcher for a local AI stack on Windows 10 and 11 (llama.cpp models, ComfyUI, Ollama, Odysseus and more). It starts, stops and watches every app, and it enforces one GPU app at a time.

## Run

`LocalAI-Manager.exe` (or `LocalAI.exe`), with `apps.toml` beside it. Press **Ctrl+K** for the command palette (or click the search box). The shortcut goes by the character the K key makes, so it is wired for Latin layouts and for Thai Kedmanee; with another layout, use the search box.

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
- **Window**: **Open** shows Odysseus inside the manager, like every app with a web page (see [Apps open inside LocalAI](#apps-open-inside-localai)).

## Apps open inside LocalAI

Every app with a `url` in `apps.toml` opens inside the manager (WebView2): ComfyUI, Wikipedia (kiwix-serve), the chat page of each llama-server model, Odysseus, Ollama. **Open** (or the command palette) shows the page under a bar with **← Back to LocalAI**, the app's name and **Reload**.

- Each app has its own view, made on its first **Open** and then only hidden. Going back and forth keeps every page as it was: the login, the open chat, the workflow on the canvas. **Reload** acts on the page that is shown.
- When an app stops (or dies) its view is dropped: the page belongs to the old process, and some apps (the Gradio voice trainer) keep showing a dead session until Reload. The next **Open** builds a fresh view. What the app stored for good in the shared profile (cookies with an expiry date, local storage) stays; a session-only cookie stays only while another app's view is still open. If that page was on screen, the window goes back to the apps.
- Logins are stored in `%LOCALAPPDATA%\LocalAI\WebView2`. Without the WebView2 Runtime, **Open** uses the browser.
- Only the app's own address (scheme, host and port) stays inside: any other link or pop-up opens in the default browser, and only `http` and `https` links are passed on. A page of one app cannot wander to another app's port.
- The views run without GPU acceleration so that the models keep the VRAM, and they go to low-memory mode while hidden.

## Chat

The **Chat** tab talks to the first running app with `chat = true`, or else to the first one in `apps.toml`. **Start** loads the model, and stops whichever GPU app is in the way (the button names it). Replies stream from llama-server's `/v1/chat/completions`. While Qwen3.6 thinks, the tail of its reasoning is shown, and it collapses to a word count once the answer starts. Chats are not saved.

## Create

The **Create** tab makes pictures and video with ComfyUI, without its node screen. It talks to the app with the id `comfy` in `apps.toml`, on that app's port. Pick one mode:

| Mode | You give | You get |
|---|---|---|
| **Image** | a description | a 1024 x 1024 picture (FLUX.2 Klein 4B, 4 steps, about 6 s once the model is loaded) |
| **Edit image** | a picture and what to change | the edited picture, about 1 megapixel (FLUX.2 Klein 4B, the picture as reference) |
| **Video** | a start picture and how it moves | an 832 x 480 clip of 49 frames, about 3 s (Wan 2.2 image-to-video 14B, 4 steps; a portrait picture is cropped to 16:9) |

**Choose picture…** opens the Windows file dialog (png, jpg, jpeg, webp). **Create** starts the job and the status line follows it ("Starting ComfyUI…", "Working… 23 s", or what went wrong in plain words). **Cancel** drops the job in ComfyUI (`/queue` and `/interrupt`). The result is shown in the tab; for a video it is the start picture plus **Open video**, which plays the file in the default player.

- If ComfyUI is not running, **Create** starts it first. When another GPU app holds the GPU, the button names it ("Stop Qwen3.6, start ComfyUI") and the click is the confirmation, as in the Chat tab. What can be checked beforehand is checked first (the picture is png, jpg or webp and at most 100 MB, the `creations\` folder can be made), so no app is stopped for a job that cannot run.
- Results are saved in `creations\` beside the exe, as `localai-<time>_00001_.png` or `localai-<time>_00001.mp4`. ComfyUI keeps its own copy in `output\LocalAI\`, and the picture you chose is copied to its `input\` folder.
- Video is RAM-bound: it peaked at 31.8 GB of 32 GB when tested, so stop the other apps first.
- The three workflows are plain ComfyUI API files in [`workflows\`](workflows), compiled into the exe. `{{prompt}}`, `{{seed}}`, `{{image}}`, `{{prefix}}` and `{{frames}}` are filled in before each job. To use other models or settings, change the files and build again.
- They need ComfyUI 0.41 or later with the custom nodes [ComfyUI-GGUF](https://github.com/city96/ComfyUI-GGUF) and [VideoHelperSuite](https://github.com/Kosinkadink/ComfyUI-VideoHelperSuite), and these files in `models\`: `unet\flux-2-klein-4b-Q8_0.gguf`, `text_encoders\qwen_3_4b.safetensors`, `vae\flux2-klein-4b_vae_diffusers.safetensors`; for video `unet\wan2.2_i2v_high_noise_14B_Q4_K_M.gguf`, `unet\wan2.2_i2v_low_noise_14B_Q4_K_M.gguf`, `loras\Wan_2_2_I2V_A14B_HIGH_lightx2v_4step_lora_260412_rank_64_fp16.safetensors` (and the `LOW` one), `text_encoders\umt5-xxl-encoder-Q3_K_M.gguf`, `vae\wan_2.1_vae.safetensors`. A missing file shows up as ComfyUI's own message in the status line.

## Training data

The right side of the **Create** tab copies files into sets, to train on later. **Import images**, **Import voice** and **Import video** open the Windows file dialog, where several files can be chosen at once (pictures: png, jpg, jpeg, webp, bmp; voice: wav, mp3, flac, ogg, m4a; video: mp4, mov, m4v, mkv, webm, avi). The copy runs in the background; the panel shows "Importing 12 of 40…", then for example `Voice set 'Alice': 14 added, 2 skipped (not audio), 1 already there.` with the set's folder under it. **Cancel** (shown while it runs) stops it within a moment, removes the file it was copying and says how far it got: `Cancelled after 3 of 40.` **Copy path** puts that folder on the clipboard and **Open folder** shows it in Explorer.

- Files go to `training\<images|voice|video>\<set>\` beside the exe. The set is named in the **Set name** box, else after the folder the files came from, else `set-YYYY-MM-DD`. Characters Windows forbids, control characters, and spaces or dots at the ends are dropped, and a device name (`CON`, `NUL`, `COM1`…) is not used, so a set can never be made outside `training\`.
- A file is taken only when its extension and its first bytes agree: a `.wav` that does not start as a RIFF/WAVE file is skipped and counted (a QuickTime `.mov` may open with any of its atoms, and a `.flac` may start with an ID3 tag). A file that cannot be read is counted as failed, with Windows' reason, not as skipped.
- A name that is in the set already is never replaced. With the same size it counts as "already there"; with another size it is "not copied: the name is taken by a different file (Recording.m4a)".
- A file name with a `:` (that is a stream of another file), one that ends with a dot or a space, and a device name (`CON.wav`) are refused before the file is opened.
- Each copy is made as `<name>.localai-part` and takes its real name only when it is whole (by a rename that never replaces). An import that is cut short (cancelled, disk full, the app closed) leaves no half file under a real name, and a `.localai-part` file left in a set's folder is deleted when the next import into that folder starts. Other files in the folder, your own `.part` files included, are never touched.
- Pictures and video are only kept: nothing on this PC trains image or video models yet. A voice set is what the [voice trainer](#voice-trainer) takes as input; **Open voice trainer** starts that app (asking first when another GPU app has the GPU, as its tile does) and shows it inside LocalAI once it is ready. That only happens where the click left you: on the Apps tab, with the voice tile still selected and no other app's page open, and within 3 minutes; otherwise it stays quiet. The button is not there when `apps.toml` has no app with the id `voice`.

## Voice trainer

The example `apps.toml` has a `voice` app for [GPT-SoVITS](https://github.com/RVC-Boss/GPT-SoVITS) v2Pro, which clones a voice from a few minutes of clips. LocalAI does not install it:

1. Download the official Windows package `GPT-SoVITS-v5-20261005.7z` from [huggingface.co/lj1995/GPT-SoVITS-windows-package](https://huggingface.co/lj1995/GPT-SoVITS-windows-package) and unpack it. The example uses `C:\GPT-SoVITS`.
2. Put `start-voice.bat` next to `apps.toml`:

   ```bat
   @echo off
   set "GSV=C:\GPT-SoVITS\GPT-SoVITS-v2pro-20261006"
   cd /d "%GSV%"
   set "PATH=%GSV%\runtime;%PATH%"
   "%GSV%\runtime\python.exe" -I webui.py en_US
   ```

3. Its web pages listen on every network interface by default. To keep them on this PC, set `server_name` to `"127.0.0.1"` in `webui.py`, `GPT_SoVITS\inference_webui.py`, `GPT_SoVITS\inference_webui_fast.py`, `tools\subfix_webui.py` and `tools\uvr5\webui.py`, and set `inbrowser=False` in `webui.py` only. The main page has no links to its sub-tools (proofreading, voice tests), so those are left to open in the browser.

Then **Import voice** on the Create tab, and paste the set's folder into step 0 of the trainer (the audio slicer's input). One clear speaker, 1 to 10 minutes in total, is enough; long recordings are fine, the trainer cuts them.

## Build

```bash
cargo build --release
```

The installer embeds that exe, so build it second:

```bash
cargo build --release --manifest-path installer/Cargo.toml
```

Then copy `installer\target\release\localai-setup.exe` to `dist\LocalAI-Setup.exe`.

The icon of both exes is `assets\app.res`, linked in by each `build.rs` (link.exe takes it as it is, so building needs no resource compiler; a `-gnu` toolchain's linker cannot read a `.res`, so there the build skips it and the exes have no file icon). It is `assets\app.rc`, which points at `icon.ico`, compiled with the Windows SDK's `rc.exe /fo assets\app.res assets\app.rc`; do that again after changing the icon. The window and taskbar icon is `assets\icon.png`, built into the manager by Slint.

`cargo test` runs:

- config validation;
- the GPU guard;
- the HTTP status parser;
- the chat stream parser and Stop;
- the setup's apps.toml merge and SHA-256;
- the model scan: GGUF header reading (control characters, damaged headers, adapters, embedding models), and the apps.toml merge (added once, hidden when commented out, Odysseus listed already, a file without a final newline, an unreadable file left untouched);
- `apps.toml` saving with its `.bak`;
- the Odysseus script against a temporary SQLite database (skipped when no Python is on PATH);
- which addresses stay inside the view of an app, and that one app's view cannot wander to another's;
- the Create tab: the placeholders of the three workflows (a prompt with quotes, newlines and backslashes arrives intact), the wiring of every workflow, the multipart picture upload, reading ComfyUI's `/history` (pending, done, failed, cancelled, out of memory), ComfyUI's refusal messages, a whole job against a stand-in ComfyUI (upload, queue, poll, download), Cancel, a job dropped in ComfyUI when it takes too long, results saved only under plain file names and never left half-written, and the checks made before anything is stopped;
- no proxy for local traffic: an agent made while `HTTP_PROXY` and the like are set still reaches 127.0.0.1 directly;
- file and folder names: device names, streams and path syntax are refused, a set name cannot leave `training\`, and a file name from the file system is read as UTF-16 (an unpaired surrogate is fine, a `:` or a trailing dot is not);
- the file dialog's result buffer (one file, several files, hundreds, stale text behind the list);
- training data: the signature of every file type (true and false cases, QuickTime atoms, FLAC with an ID3 tag), the set name rules, the result line (a name taken by a different file, a cancelled import), an import into a temporary folder (counts, wrong type skipped, nothing overwritten, a different file under the same name told apart by size, unsafe and non-UTF-8 names, a file that cannot be read, a copy that is cut short or cancelled leaves no file, old `.localai-part` files cleared while other files stay, publishing never replaces, nothing made outside the training folder), and the worker's progress, result and Cancel;
- the wait behind **Open voice trainer**: it opens the page only where the click left the user, and runs out after three minutes;
- a test that a job-object kill takes down grandchild processes.

Three more tests need a running ComfyUI on port 8188 (or `LOCALAI_COMFY_PORT`) and the GPU, so they are skipped unless asked for: `cargo test -- --ignored --test-threads=1` makes one picture, edits it, and renders a 17-frame clip from it (the clip is skipped when less than 14 GB of RAM is free).

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
- **`detect.rs`**, **`odysseus.rs`**, **`web.rs`**: the model scan, the Odysseus link and the embedded app pages described above.
- **`create.rs`**, **`train.rs`**, **`pick.rs`**: the Create tab's worker thread (ComfyUI's HTTP API, the three workflows), the training data import, and the Win32 file dialog (one picture, or many files).
- **`names.rs`**, **`net.rs`**: which names are safe to create on Windows, and the HTTP agent for local apps (never through a proxy).
- Desktop/terminal apps (`group = "app"`) are launch-only and keep running after the manager quits.

## Startup

The software renderer was chosen on purpose. On this machine the first paint takes about 85 ms, against about 330 ms with the OpenGL (femtovg) renderer. The figures were measured externally, from process start to the first painted frame.

`LocalAI-Manager.exe --bench-startup` prints in-process timings to stdout and exits.

## License

MIT, see [LICENSE](LICENSE). The user interface is built with [Slint](https://slint.dev), used under the Slint Royalty-free License 2.0 (attribution badge at the top of this page). Other dependencies are MIT or Apache-2.0 licensed.
