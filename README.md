# Staw

Decision memory for teams and their agents.

Staw captures the decisions a team makes — ADR-style records with
context, alternatives, and consequences — where the work happens, and keeps
them as a living graph: decisions supersede earlier ones, cross-reference
each other, and stay comparable long after the conversation that produced
them is gone. Humans and AI agents read and write the same memory: a web UI
for people, an [MCP](https://modelcontextprotocol.io) endpoint for agents,
and one REST API under them both.

## Quickstart

```sh
docker compose up --build
docker compose exec staw staw-server token mint   # prints your bearer token
```

(`token list` / `token revoke <id>` manage tokens from the host; `--user
<handle>` provisions teammates without an identity provider — each then
manages their own via `/api/v1/tokens`.)

Authentication is always on; the mint command prints a `cvg_…` secret to
your terminal (never to logs, where collectors would keep it). Then:

- **Web UI**: <http://127.0.0.1:8080> — paste the token at the sign-in
  screen; it's exchanged for an `HttpOnly` session cookie (the secret is
  never stored in the browser).
- **MCP** (Claude Code shown; any MCP client works):

  ```sh
  claude mcp add --transport http staw http://127.0.0.1:8080/mcp \
    --header "Authorization: Bearer cvg_..."
  ```

  OAuth-capable MCP clients (claude.ai connectors) need no header: the
  server is a full OAuth authorization server (discovery, dynamic client
  registration, PKCE) — add the URL, sign in when the browser opens, done.
  Connector grants appear in the settings UI as `connector:…` tokens;
  revoking one cuts that connector off. Set `[auth] session_secret` (and
  `public_url` behind a proxy) — registered clients are signed by that
  key, so an unset (per-boot random) key orphans them on restart.

  Agents get the full palette: decisions (`decision_add` with
  supersession + evidence anchors, `decision_get`, `decision_list`),
  evidence ingest (`session_ensure`, `message_add`), and project mapping
  (`project_match`/`project_bind`/`project_dismiss`).

- **Claude Code integration** (`staw-cli`): one command wires it all —

  ```sh
  curl -fsSL https://raw.githubusercontent.com/staw-ai/staw/main/install.sh | sh
  staw init    # credentials → hooks → MCP registration, once per machine
  ```

  (The binary lands in `~/.staw/bin` with a `~/.local/bin` symlink;
  releases are checksum-manifested and minisign-signed. Building from
  source works too: `cargo install --path crates/staw-cli`. Coming from
  converge? See [upgrading from converge](docs/upgrading-from-converge.md).)

  After that, opening any repository in Claude Code suggests a project
  binding in-session (the human answers in conversation; hooks write the
  committed `.staw` marker), session transcripts sync into the
  evidence layer automatically, and every session starts with the
  project's decision index injected. `staw project init` is the
  manual fallback for binding from the terminal.

- **REST**: `GET`/`POST` `/api/v1/{groups,projects,decisions,users,agents}`
  with cursor pagination (`?limit=&cursor=` → `{items, next_cursor}`;
  signals also page forward, oldest first, with `?since=`),
  atomic edit batches via `PATCH`, graph reads at `/decisions/{id}/edges`,
  and read-only relation projections (`/groups/{id}/decisions` is the
  group-wide feed).

> **Security note:** every surface except the health probe, the sign-in
> endpoints, and the static assets requires a credential — a bearer token
> (agents, CLI) or the session cookie (browser). The compose file
> publishes the port on the host loopback only — a sane default until you
> put TLS in front. Set `STAW_AUTH__SESSION_SECRET` to keep browser
> sessions across restarts.

### Identity-provider sign-in (optional)

The auth core needs no egress — closed-contour installations work with
tokens alone. For team sign-in, point `[auth.oidc]` at any OIDC issuer
with standard discovery (Keycloak, Authentik, Dex, GitLab, Forgejo…);
GitHub is the one built-in special case (it speaks OAuth2, not OIDC):

```toml
[auth.oidc]
provider = "github"        # or any name for a generic issuer ("keycloak", …)
# issuer = "https://sso.example.com/realms/main"   # generic providers only
client_id = "…"
client_secret = "…"
public_url = "https://staw.example.com"  # register {public_url}/auth/callback
allowed = ["singulared"]   # optional handle allowlist; absent = the IdP decides
```

The login screen then offers "Sign in with …" beside the token paste.

Authorization is group-based: every group has exactly one owner and a
flat member list, and **membership is visibility** — you see the groups
you own or belong to, and nothing else exists for you (projects,
decisions, sessions, and signals all inherit the group's boundary;
expert signal detection never crosses it). The owner manages membership:
`POST /api/v1/groups/{id}/members {"handle": "..."}` adds an existing
user (signing in once is what creates a user — there are no pending
invites yet), `DELETE .../members/{user}` removes one (members can
remove themselves). Pre-ACL databases are adopted at boot: ownerless
groups become the deployment user's. Owner transfer and invitations for
not-yet-registered users are the next step of the ownership model.

## Slack bot

`staw-slack` answers questions about your team's decisions from Slack.
Mention it in a thread of a channel that is connected to a Staw project —
`@Staw what did we decide about the queue?` — and it answers you in a
direct message, from the decisions that *your* Staw account can see. The
thread gets a reaction and nothing else. It is a separate process and one
more MCP client of Staw: it reads Staw through `/mcp` with the token of
the person who asks, and its own model reads the thread. It does not
record decisions yet.

It is for your team's own workspace: an internal Slack app that you
create from the manifest in this repository.

1. **Create the Slack app.** At <https://api.slack.com/apps> choose
   *Create New App → From a manifest*, pick your workspace, and paste
   [`crates/staw-slack/manifest.yaml`](crates/staw-slack/manifest.yaml).
   It asks for the bot scopes `app_mentions:read`, `channels:history`,
   `groups:history`, `chat:write`, `reactions:write` and `users:read`;
   subscribes to `app_mention` and `app_home_opened`; and turns on
   interactivity (the Disconnect button) and Socket Mode — the bot
   connects out to Slack, so it needs no public URL.
2. **Make two tokens.** *Basic Information → App-Level Tokens → Generate
   Token and Scopes* with the scope `connections:write` gives the
   `xapp-…` token for Socket Mode. *Install App → Install to Workspace*
   gives the bot token, `xoxb-…`.
3. **Invite the bot** to each channel you want to map (`/invite @Staw`).
   A channel's id is at the bottom of its details in Slack (`C…`); a Staw
   project's id is in its URL in Staw (`#/project/<id>`).
4. **Write `staw-slack.toml`** next to `compose.yaml`:

   ```toml
   [slack]
   bot_token = "xoxb-…"
   app_token = "xapp-…"

   [staw]
   # Where people open Staw: the bot's links point here. Behind a proxy,
   # Staw's public URL — /mcp answers only to Staw's own host names.
   url = "http://127.0.0.1:8080"

   [model]
   provider = "anthropic"        # or "openai": any OpenAI-compatible runtime, with base_url
   model = "claude-sonnet-5-5"
   api_key_cmd = "cat /run/secrets/anthropic"   # or api_key = "…"
   # Per model call: timeout_secs = 120, max_tokens = 4096.
   # Per request: rounds = 8 model calls, budget_secs = 180.
   # thread_chars = 60000: how much of a thread the model reads.

   [[channels]]
   channel = "C0123ABCD"                     # the Slack channel
   project = "01M2P205AK2GMNWSS0BQGM6EN9"    # the Staw project

   [store]
   path = "/var/lib/staw-slack/people.json"  # people's Staw refresh tokens
   ```

   Any key can come from the environment instead
   (`STAW_SLACK_SLACK__BOT_TOKEN`, `STAW_SLACK_MODEL__API_KEY`, …). The
   bot reads `/etc/staw/slack.toml`, `./staw-slack.toml`, and the file
   named by `--config` or `$STAW_SLACK_CONFIG`. With no configuration at
   all it says what to set and exits; at start it checks both Slack
   tokens, that Staw answers, and the model's key.
5. **Run it next to Staw:**

   ```sh
   docker compose --profile slack up --build
   ```

   The `staw-slack` service runs from Staw's own image, only under its
   profile — `docker compose up` alone starts what it always did. It
   shares the `staw` container's network, so the `url` above is right
   for this compose file. Without Docker:
   `cargo run --release -p staw-slack -- --config staw-slack.toml`.

**Connecting.** The first time a person asks, the bot sends them a link
and a code. They approve it in Staw, signed in as themselves, and the bot
answers "Connected as …". Their Staw settings then list the token as
`connector:Slack <workspace>: @<name>`. From then on each of their
requests runs with their own token, never anyone else's, so an answer
holds only what they can see in Staw. The App Home shows whom they are
connected as, with a Disconnect button: the bot deletes the token and
says how to revoke it in Staw.

**What mapping a channel means.** The thread that a person asks in, up to
`thread_chars` of it, goes to the configured model provider. Answers go
only to the person who asked; the channel sees a reaction. A channel
shared with another organization (Slack Connect) is refused, and so is a
channel that is not mapped. In this version nothing from Slack is written
to Staw.

## Development

Rust 1.88+. Docker is needed for the dev database and integration tests,
never for building (queries compile against the committed `.sqlx/` cache).

```sh
cargo check                 # no database needed — queries check against .sqlx/
cargo test                  # integration tests boot throwaway Postgres containers

cargo xtask dev             # the full stack: Postgres + server, dev token printed
cargo xtask dev --web       # …plus the web app, served same-origin
cargo xtask db              # just the dev Postgres: boots, migrates, prints DATABASE_URL
cargo xtask prepare         # regenerate .sqlx/ after changing queries or schema
cargo xtask prepare --check # verify .sqlx/ is current (CI)

# the web app, live-reloading against a locally running staw-server:
rustup target add wasm32-unknown-unknown && cargo install trunk
cd crates/staw-web && trunk serve --features api   # http://127.0.0.1:8081
```

Configuration is layered per key — `/etc/staw/config.toml` ←
`$XDG_CONFIG_HOME/staw/config.toml` ← `./staw.toml` ←
`$STAW_CONFIG` ← `STAW_*` env (nested keys use `__`, e.g.
`STAW_LOG__FILTER`, `STAW_WEB__ASSETS`). The only required setting
is `database_url`.

## Workspace

| Crate | What it is |
|---|---|
| `staw-storage` | The domain model (groups → projects → decisions, the decision graph, users/agents) and the per-resource `Storage` traits the product is written against. |
| `staw-storage-postgres` | The PostgreSQL backend — compile-time-checked queries over the schema in `migrations/`. |
| `staw-server` | The product binary: REST under `/api/v1`, the stateless `/mcp` endpoint, and same-origin serving of the web bundle. |
| `staw-client` | Typed HTTP client over the same domain types — compiles natively (the future CLI) and to wasm (the web UI). |
| `staw-ui` | The component library (Leptos): design tokens, atoms, molecules. |
| `staw-web` | The web app (Leptos CSR → wasm), composed from `staw-ui`, talking to the API through `staw-client`. |
| `staw-slack` | The Slack bot: answers questions from Slack threads in a direct message, through `/mcp` with the asking person's token (see [Slack bot](#slack-bot)). |
| `xtask` | Dev tooling (`cargo xtask …`): a throwaway dev Postgres and sqlx offline-cache generation. |

## Design, in one paragraph

Everything above the storage seam is backend-agnostic: the server, client,
and web app are written against the per-resource `Storage` traits and a
shared wire contract (the domain types *are* the wire format), so a
contract change is a compile error, not a runtime surprise. Ids are ULIDs
(stored as native `uuid`; `order by id` is capture order), `superseded` is
always derived from supersession edges rather than stored, instants are
server-assigned RFC3339 UTC only, edits apply as atomic batches of sparse
operations, and every surface that creates users or agents resolves them by
natural key (`ensure`), never scan-then-create.

## License

[MIT](LICENSE). Vendored fonts (Geist, Geist Mono) are under the SIL Open
Font License — see `crates/staw-ui/style/fonts/OFL.txt`.
