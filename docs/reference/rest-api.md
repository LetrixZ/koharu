---
title: REST API
description: Automate Koharu's desktop workflow over a localhost HTTP API.
---

# REST API

Koharu ships an optional embedded REST API so external tools can drive the same
workflow the desktop interface offers: creating projects, importing images,
running the translation pipeline, and exporting translated pages.

The API is served **inside the desktop application process**: it shares the
open project, processing jobs, and rendered canvas, so an API call is
indistinguishable from using the window. It is disabled by default and, by
default, only ever listens on `127.0.0.1`.

## Enable the API

Open **Settings → API**, flip the server toggle on, and press **Apply changes**.
The server listens on the configured **host** (`127.0.0.1` for loopback only;
`0.0.0.0` accepts connections from the local network) and TCP **port**
(default `4000`, any value from 1–65535). The **API key** is optional: when a
key is set, every request must present it; when it is empty, requests are
accepted unauthenticated. Applying the settings restarts the server
immediately.

## Discover the endpoint

On startup — and again whenever the API settings change in the UI — Koharu
logs the listening address and writes the host, port, and token to
`~/.koharu/api.json` so external tools can find the endpoint without parsing
logs. The token is `null` when no key is configured. The file is removed when
the server is disabled:

```json
{
  "host": "127.0.0.1",
  "port": 4000,
  "token": "9f4b2a1c7d3e5f6a"
}
```

When a key is configured, send it on every request:

```http
Authorization: Bearer <token>
```

## Run in the background

Start Koharu with `--background` to run it without a window — the app stays
alive as a background server, so the REST API keeps working with no visible
interface:

```console
koharu --background
```

The menu bar icon is always present while the app runs. Clicking it opens the
menu: **Show window** / **Hide window** toggles the main window (its label
reflects the action), and **Quit Koharu** exits the app. Closing the window
hides it to the tray instead of quitting. While the app has no visible window
it runs in background mode — on macOS the Dock icon is hidden too and only the
menu bar icon remains, until the window is restored.

## Workflow overview

The complete automation flow maps to four calls. After the app has finished
initializing its models (see `GET /v1/status`), an external client can:

1. **Create a project** with `POST /v1/projects`.
2. **Load images** by uploading files to
   `POST /v1/projects/{name}/images`.
3. **Run the pipeline** — start translation work and poll its job.
4. **Export** — download the translated pages as a ZIP archive.

## Endpoints

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/v1/status` | Model initialization state, current project, running job |
| `GET` | `/v1/projects` | List stored projects |
| `POST` | `/v1/projects` | Create a project |
| `GET` | `/v1/projects/{name}` | Open a project and return its state |
| `DELETE` | `/v1/projects/{name}` | Delete a stored project |
| `POST` | `/v1/projects/{name}/images` | Upload pages as `multipart/form-data` |
| `POST` | `/v1/projects/{name}/pipeline` | Start a pipeline job |
| `GET` | `/v1/jobs/{id}` | Poll job progress and result |
| `POST` | `/v1/jobs/{id}/stop` | Cancel a running job |
| `GET` | `/v1/translation/models` | List available translation models |
| `GET` | `/v1/translation/languages` | List supported target languages |
| `GET` | `/v1/translation/preferences` | Current translation model and target language |
| `PUT` | `/v1/translation/preferences` | Update the model and/or target language |
| `GET` | `/v1/projects/{name}/export.zip` | Download rendered pages as ZIP |

Errors return a matching status code with a JSON body:
`{"error": "<human-readable message>"}`.

## App state

### `GET /v1/status`

```json
{
  "initialized": true,
  "processing": false,
  "project": null
}
```

- `initialized` — models and pipeline are ready. Automation should wait for
  `true` before starting work.
- `processing` — a pipeline job is currently running.
- `project` — the open project, or `null`; the same shape as the
  `GET /v1/projects/{name}` response.

### Project shape

```json
{
  "name": "demo",
  "revision": "42",
  "active_page": "01J9R2P4X...",
  "can_undo": false,
  "can_redo": false,
  "pages": [
    {
      "id": "01J9R2P4X...",
      "label": "page1.png",
      "size": { "width": 1200, "height": 1800 },
      "source_asset": "blob:...",
      "layer_count": 2,
      "text_layers": 2,
      "translated_layers": 0,
      "translated": false
    }
  ]
}
```

Each page reports its translation state: `text_layers` is the number of text
elements on the page, `translated_layers` how many of those carry a
translation, and `translated` is `true` when every text layer is translated
and at least one exists. Pages that were never processed have no text layers
and report `translated: false`, so they are picked up by a fresh pipeline run.

Opening a project selects it for all later calls in the same way that
clicking a project in the desktop library does: the window switches to it and
pipeline/export calls operate on it. `GET /v1/projects/{name}` is therefore
both a read and a selection step.

## Create a project

```http
POST /v1/projects
Authorization: Bearer <token>
Content-Type: application/json
```

```json
{
  "name": "my-translation"
}
```

Returned with status `201 Created` and the project shape. Creating a project
that already exists returns `409 Conflict`. Images are added separately with
`POST /v1/projects/{name}/images`.

## Upload images

```http
POST /v1/projects/{name}/images
Authorization: Bearer <token>
Content-Type: multipart/form-data
```

Upload one or more files as `multipart/form-data`; each uploaded file becomes
a page appended to the end of the project. The page label is the upload's file
name (or `page-<n>.<ext>` when none is given):

```console
curl -X POST -H "$AUTH" \
  -F "images=@/abs/path/01.png" \
  -F "images=@/abs/path/02.png" \
  $BASE/v1/projects/demo/images
```

The field name is free-form; only file parts are imported. Sending no files
returns `400 Bad Request`. Supported image formats match the desktop importer:
PNG, JPEG, WebP, CBZ/ZIP archives, RAR archives, and PDF.

## Configure translation

The API mirrors the desktop's translation preferences so an external app can
remember the user's model and target language.

```http
GET /v1/translation/models
```

Returns the same model choices as the desktop picker, including provider,
available quantizations, and vision/reasoning support:

```json
[
  {
    "provider": "local",
    "model": "qwen2.5-14b-instruct-q4_k_m.gguf",
    "name": "Qwen 2.5 14B Instruct (Q4_K_M)",
    "quantizations": [{ "id": "q4_k_m", "name": "Q4_K_M" }],
    "vision": true,
    "reasoning": true
  }
]
```

```http
GET /v1/translation/languages
```

Lists every supported target language as `{ "tag": "es-ES", "name": "Spanish" }`
entries; `tag` is the value to send back.

The selected model is stored as a `ModelSelection` (`provider`, `model`,
`quantization`, `vision`, `reasoning`); pick the first quantization of a model
like the desktop does. Read the current preference, then update either half of
it with `PUT /v1/translation/preferences`:

```http
PUT /v1/translation/preferences
Authorization: Bearer <token>
Content-Type: application/json
```

```json
{
  "model": { "provider": "openrouter", "model": "anthropic/claude-3.5-sonnet", "quantization": null, "vision": true, "reasoning": true },
  "target_language": "es-ES"
}
```

Both fields are optional; omitted ones keep their current value. The response
is the resulting preference. Unknown language tags return `400 Bad Request`.

## Run the pipeline

```http
POST /v1/projects/{name}/pipeline
Authorization: Bearer <token>
Content-Type: application/json
```

```json
{
  "operation": "full",
  "pages": [],
  "elements": []
}
```

- `operation` — `full`, `detection`, `ocr`, `translation`, or `inpainting`;
  defaults to `full`, which runs the complete translation workflow described
  in [Process Pages](/workflow/process-pages/).
- `pages` — restrict processing to these page IDs. Automation typically reads
  the project state first, collects the page IDs where `translated` is
  `false`, and passes exactly those so already-translated pages are not
  reprocessed.
- `elements` — restrict processing to these text element IDs; cannot be
  combined with `pages`.

```json
{
  "job": "a1b2c3d4-...",
  "url": "/v1/jobs/a1b2c3d4-..."
}
```

Only one pipeline job may run at a time; starting another returns
`409 Conflict`. If a translation provider or model is not configured, the job
finishes with `state: "failed"` and an `error` message.

## Poll job progress

```http
GET /v1/jobs/{id}
```

```json
{
  "id": "a1b2c3d4-...",
  "state": "running",
  "completed": 3,
  "total": 8,
  "page": "01J9R2P4X...",
  "stage": "ocr",
  "model": "koharu-ocr-...",
  "error": null
}
```

`state` is `running`, `finished`, `failed`, or `stopped`. Poll until it
leaves `running`; `total` is the number of page × stage units and `completed`
counts finished units. Call `POST /v1/jobs/{id}/stop` to cancel.

Finished jobs stay queryable: the endpoint returns their terminal state
instead of disappearing. The most recent 16 jobs are retained for the current
project; starting a new pipeline rolls off the oldest finished job, and
switching or closing the project clears the history.

## Export translated pages

```http
GET /v1/projects/{name}/export.zip
```

Returns an `application/zip` archive of the project's pages rendered as
high-quality PNGs, named `0001_<label>.png` and so on in project order.
Pass `?pages=<id>,<id>` to export a subset. The ZIP contains the same output
as the desktop [flattened export](/workflow/export/).

## Example: complete automation flow

```console
BASE=http://127.0.0.1:4000
# an empty bearer header is harmless when no key is configured
AUTH="Authorization: Bearer $(jq -r '.token // empty' ~/.koharu/api.json)"

# wait for model initialization
until curl -fsS -H "$AUTH" $BASE/v1/status | jq -e .initialized; do sleep 1; done

# remember the user's model and target language preference
MODEL=$(curl -fsS -H "$AUTH" $BASE/v1/translation/models | jq -c '.[0] | {provider, model, quantization: (.quantizations[0].id // null), vision, reasoning}')
curl -fsS -X PUT -H "$AUTH" -H "Content-Type: application/json" \
  -d "{\"model\": $MODEL, \"target_language\": \"es-ES\"}" \
  $BASE/v1/translation/preferences > /dev/null

curl -fsS -X POST -H "$AUTH" -H "Content-Type: application/json" \
  -d '{"name": "demo"}' \
  $BASE/v1/projects

curl -fsS -X POST -H "$AUTH" \
  -F "images=@/abs/path/page1.png" \
  -F "images=@/abs/path/page2.png" \
  $BASE/v1/projects/demo/images

# translate only the pages that are not translated yet
PAGES=$(curl -fsS -H "$AUTH" $BASE/v1/projects/demo | jq -c '[.pages[] | select(.translated == false) | .id]')
JOB=$(curl -fsS -X POST -H "$AUTH" -H "Content-Type: application/json" \
  -d "{\"operation\": \"full\", \"pages\": $PAGES}" \
  $BASE/v1/projects/demo/pipeline | jq -r .job)

until [ "$(curl -fsS -H "$AUTH" $BASE/v1/jobs/$JOB | jq -r .state)" != "running" ]; do sleep 2; done

curl -fsS -H "$AUTH" -o demo.zip $BASE/v1/projects/demo/export.zip
```

## Notes

- The API is a local automation surface, not a remote service. When it binds
  `127.0.0.1` only the host machine can reach it; setting the host to
  `0.0.0.0` exposes it to the whole local network, so set an API key before
  publishing and keep it secret. With no key configured the API is
  unauthenticated.
- The enabled state, host, and port persist in the `[api]` section of Koharu's
  configuration file; the token is stored as a device secret.
- Automation shares state with the desktop window. Opening, importing, and
  processing update the live project and canvas just like interacting in the
  UI; concurrent manual edits are rebased the same way pipeline output is.
- The application writes `~/.koharu/api.json` with restrictive permissions
  (`0600` on Unix) and rewrites it whenever the server restarts.