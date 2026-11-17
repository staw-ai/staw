# Review request: bringing decisions into Staw from Slack, Jira and similar systems

## What this is

This is a design proposal for a new part of a product called Staw. Nothing in
it is built. An AI assistant working in the product's repository drafted it,
and the product owner, who is sending it to you, has not accepted it. We want
an independent review of the approach before any code is written.

Please read it as an engineer who has run chat and issue-tracker integrations
in production and who would have to maintain this one for years. A review that
shows where the design is wrong, or where it will hurt later, is worth more to
us than one that confirms it. You cannot see the repository, so this text
should carry everything you need. Where it does not, say what you assumed. A
longer implementation brief exists; this text is its design, condensed.

The questions we most want answered, and the form of answer that helps us
most, are in the last section.

## The system today

Staw is a decision memory for software teams and their AI coding agents. It
keeps the history of each project's decisions as ADR-style records, and it
tells people when a new decision conflicts with an earlier one.

- **Groups, projects, decisions.** A group owns projects, and a project owns
  decisions. Group membership is the access boundary. Every read and every
  checked write carries an explicit scope: a user, for whom the access check
  applies, or the system, for trusted internal callers.
- **A decision** has a title, a summary, optional context and consequences,
  the alternatives that lost, a status, one or more authors, and evidence.
  The statuses are `accepted`, `draft`, `proposed` and `rejected`;
  `superseded` is derived from a "supersedes" edge between two decisions.
- **An author** is a person, an agent, or a person acting through an agent.
  A decision must have one.
- **Evidence is mandatory and immutable.** It is either recorded messages of
  a conversation or a code range pinned to a commit.
- **A session** holds one conversation. It has a kind (`transcript`, `slack`,
  `pr` and `incident` exist; agent sessions produce `transcript`) and a
  natural key `(kind, external id)`. Its **messages** are append-only. Each
  has a speaker as a display string, because speakers need not be Staw
  users, a body, a time and a position. Today a message is identified by its
  position in the session.
- **The archive.** Each project chooses whether whole conversations are
  stored, or only the messages that decisions cite.
- **Two entry points for decisions.** The web UI calls a REST endpoint. AI
  coding agents, running in harnesses such as Claude Code, call an MCP tool.
  The MCP tool requires evidence and records the author as the person acting
  through the agent. After a write, both entry points start the conflict
  search in the background, where it never blocks the write, and look up
  similar decisions to show the caller.
- **Staw Expert** is the LLM layer on the server. A job is a named model
  call with a schema-constrained reply, bound to a model in configuration. A
  job with no binding is disabled, and every expert feature fails open. Two
  jobs exist. `signals` compares a new decision with related decisions in
  all projects of the group and raises a **signal** when it finds a
  conflict. `ask` answers a question from the group's decisions and reads
  only what the asker may see. The expert cannot call tools yet.
- **Signals** are delivered into agent sessions today: a hook polls a ledger
  on the server at each prompt, and a receipt is kept per (signal, user,
  session). The person gives a verdict on each signal.
- **Stack and deployment.** Rust, axum, PostgreSQL through sqlx, an MCP
  server, a multi-provider model client, a web UI compiled to wasm. Storage
  sits behind repository traits. There is a self-hosted server and a hosted
  service, and the hosted binary composes the same router. The design
  assumes that more than one replica may run. One external system is
  integrated so far: GitHub, read-only, to check code evidence against a
  repository.
- **Scale.** The product is early (version 0.2) and we have no volume
  figures. If an objection depends on scale, say at what scale the design
  breaks.

## The task

Teams also decide things where no agent runs: in Slack threads and Jira
issues, and later in pull-request discussions, Teams and meeting transcripts.
The product owner asked for a layer, or two, between Staw Expert and these
places, and named these cases:

1. **A Slack bot.** A person in a channel asks the bot to record a specific
   decision from the conversation.
2. **A Slack source.** A periodic job fetches new conversations, analyzes
   them and extracts the decisions that nobody sent.
3. **Jira.** The expert fetches new issues, and a person can tag the expert
   from inside Jira.
4. **Many more** bots and sources of the same kind later.
5. **MCP.** In addition, the expert can use MCP servers.

The outcome to avoid is one copy of the ingestion pipeline per system and
per way in.

## Constraints

**Accepted decisions of the team:**

- A decision cannot exist without evidence, and evidence never changes. The
  archive of whole conversations is optional per project.
- A decision must have an author.
- A message is identified by its position in its session.
- Text shown to a person is kept apart from diagnostic logs, and logs are
  sanitized.

**The team's product design, proposed and not yet final:**

- The path a decision takes depends on who formulated it, a person or an
  agent, and not on the channel it came through.
- A decision that duplicates an existing one is added to that one as further
  evidence. This happens before the conflict search.
- A "value filter" will score decisions that an agent formulated and drop
  those below a threshold. Decisions that a person formulated skip it. It is
  designed and not built.
- A signal is a conflict. It goes to the author of the new decision, a bot
  may be what shows it, and the person chooses between the two decisions.
- After an agent session ends, the expert will read its transcript and
  capture what was not recorded. This is not part of the task, but the
  design should be able to carry it.

**Rules of the repository:**

- An integration does nothing without configuration: no network call and no
  route. Configuration that is present and unusable is a startup error.
- No new infrastructure. Postgres tables and async tasks, with no queue
  service and no plugin runtime.
- Tests never reach the network.

**Facts about the external systems.** The Slack facts were checked against
Slack's documentation on 2026-11-14. The Jira fact was not checked again.
Correct us if you know any of them to be different.

- Slack limits `conversations.history` and `conversations.replies` to one
  request a minute and 15 messages a request for commercially distributed
  apps outside its Marketplace (changelog of 2025-05-29). An app that a
  customer builds for its own workspace keeps the normal limits.
- Slack's Events API expects an acknowledgement within three seconds and
  delivers the event again when it does not get one.
- Jira sends comment events separately from issue events. A mention is a
  node of the document format that carries an account id.

## The proposal

Two layers and one capability.

```
   Slack      Jira     (GitHub PRs, Teams, …)      any script
     │          │               │                      │
┌────▼──────────▼───────────────▼────────┐             │
│ ADAPTERS            one per system     │             │
│   bot role      someone asks           │             │
│   reader role   nobody asks            │             │
│   knows   the API, credentials, ids    │        REST entry
│   never   models, statuses, writes     │             │
└───────────────────┬────────────────────┘             │
                    │  conversation + ask              │
┌───────────────────▼──────────────────────────────────▼───┐
│ INTAKE              one pipeline, the only writer        │
│   authorize → capture → match → record → detect          │
│   owns   the path, the policy, idempotency               │
└────────┬──────────────────────────────┬──────────────────┘
         │ jobs                         │ storage
┌────────▼─────────────────┐   ┌────────▼────────────────────┐
│ STAW EXPERT              │   │ sessions, messages,         │
│   capture   (new)        │   │ decisions, signals          │
│   signals, ask (exist)   │   └─────────────────────────────┘
│   tools ─────────────────┼──▶ MCP servers: read-only lookups
└──────────────────────────┘
```

Answers and signals travel back up through the adapter.

### Adapters

One adapter per external system, compiled into the server. An adapter knows
that system and nothing else: its API, credentials, events, ids and markup.
It turns what it sees into the intake's types and renders the intake's
answer back. It never calls a model, never chooses a status or an author,
and never writes a decision, a session or a message.

An adapter has up to two roles:

- **Bot role.** A person addressed Staw inside the system. The adapter hands
  the request over and posts the answer where it was asked. Later it also
  delivers signals and takes the verdict.
- **Reader role**, the owner's "source". Nobody asked. The adapter hands
  over a conversation once it has been quiet for a settle time. Where the
  data comes from is the adapter's business: a poll with a cursor for Jira,
  a buffer that events fill for Slack.

The roles are not two layers. The Slack bot and the Slack reader share the
client, the credentials, the thread fetch, the identity mapping and the
channel binding, and differ only in what starts them. The Slack reader and
the Jira reader share everything after the hand-over. So the cut runs
between what is specific to one system and what is common to all.

Three shared tables serve every adapter:

- **binding**: a place in the external system (a Slack channel, a Jira
  project) tied to exactly one Staw project. Its key is the adapter, the
  external workspace and the place. It also holds whether background reading
  is on, the reader's cursor state and a lease.
- **identity**: an external account tied to one Staw user. Only a person who
  asks needs one. The speakers of a conversation stay display strings.
- **inbox**: incoming events, unique by adapter and event id, with a lease
  and an attempt count. It makes a webhook delivery idempotent and lets its
  processing survive a restart.

An adapter may also own private tables, such as the Slack message buffer.

### The contract between an adapter and the intake

```rust
/// What an adapter hands over. It becomes a session and its messages.
pub struct Conversation {
    pub kind: SessionKind,
    /// Stable and linkable: the canonical permalink of the thread.
    pub external: String,
    pub title: String,
    /// Oldest first.
    pub lines: Vec<Line>,
}

pub struct Line {
    /// The system's own id for this message (Slack `ts`, a Jira comment id).
    pub external: String,
    /// Display name. Speakers need not be Staw users.
    pub speaker: String,
    /// Plain text, with mentions and links made readable.
    pub body: String,
    pub sent_at: Option<OffsetDateTime>,
}

/// Facts only. The adapter says who asked and where; it does not judge
/// what kind of request it was.
pub enum Ask {
    /// A person addressed Staw. `text` is what they wrote without the
    /// mention; `at` is the `Line::external` where they wrote it.
    Said { by: UserId, text: String, at: String },
    /// Nobody asked: a reader handed this over.
    Unprompted,
}

pub struct Submission {
    /// "slack". Also the agent name its writes carry.
    pub adapter: &'static str,
    /// From the binding.
    pub project: ProjectId,
    pub conversation: Conversation,
    pub ask: Ask,
}

/// What happened, for an adapter to render in its own markup.
/// `Recorded` is a decision's id and title.
pub struct Outcome {
    /// New decisions.
    pub recorded: Vec<Recorded>,
    /// Existing decisions that gained this conversation as evidence.
    pub attached: Vec<Recorded>,
    /// Near matches worth showing the person.
    pub similar: Vec<(DecisionId, String)>,
    /// The reply to a question.
    pub answer: Option<String>,
    /// Why nothing was recorded, in the person's words.
    pub note: Option<String>,
}

impl<S: Storage + 'static> Intake<S> {
    pub async fn submit(&self, submission: Submission) -> Result<Outcome, StoreError>;
}
```

What a person wrote is interpreted in one shared place, never per adapter.
At first this is a small fixed grammar that needs no model:

- `help`
- `bind <project>` and `unbind`
- `record: <statement>`: the person names the decision
- `record` alone: a request to capture what the conversation decided
- anything else is a question

Telling a named decision from a request in free speech is a later model
call behind the same function.

### The intake pipeline

One service. It is the only path from an adapter to storage and to the
expert. Steps 1 to 5 write nothing.

1. **Authorize.** When a person asked, that person must be able to see the
   project, and every read and write of the submission runs under that
   person's scope, so a bot never writes where its asker cannot. When
   nobody asked, the scope is the system's, and nothing outside the
   binding's project is touched.
2. **Interpret** the ask into a mode: named, requested, question or
   unprompted.
3. **A question** is answered by the existing `ask` job under the asker's
   scope. Nothing is recorded.
4. **Load what is known**: the decisions that already cite this
   conversation, and the nearest decisions of the project.
5. **Capture.** For a named decision there is one draft. With a model, the
   statement is shaped into a full record from the conversation, and the
   model must not change the choice. With no model, the statement is
   recorded as written. For a request and for an unprompted conversation,
   the capture job drafts zero or more decisions, and with no model nothing
   is written. The value filter will run here.
6. **Match.** A draft that the job marked as an existing decision adds its
   cited lines to that decision as evidence, and nothing new is recorded.
   The intake never supersedes a decision on a model's word. A replacement
   shows up as a near match in the answer and as a conflict signal, and a
   person supersedes.
7. **Record evidence.** Ensure the session, then store the lines: the whole
   conversation when the project archives, only the cited lines when it
   does not.
8. **Write** each new decision with the status and author from the policy
   table.
9. **Detect.** Start the existing conflict search and the lookup of similar
   decisions.
10. **Return** an outcome that the adapter renders.

The policy is one table, so a product change edits it and touches no
adapter:

| What was asked | Formulated by | Value filter | Status | Author | Runs as |
| --- | --- | --- | --- | --- | --- |
| A named decision: "record: we use Postgres for the queue" | the person | no | `accepted` | the person, through the adapter's agent | the asker |
| A request to capture: "record what we decided" | the agent | yes, once it exists | `accepted` | the person, through the adapter's agent | the asker |
| Nothing; the reader handed it over | the agent | yes, once it exists | `proposed` | the expert agent | the system, in the binding's project only |
| A question | — | — | nothing is recorded | — | the asker |

### The capture job

One model call, built like the existing `signals` job: a conversation in,
drafted decisions out, no storage and no side effects.

- **Request**, with slow-changing fields first so that a provider can cache
  the prefix: the project's name and description, the known decisions from
  step 4 (id, title, summary), the numbered lines, and the ask last.
- **Reply**: a scratchpad, then drafts. Each draft has a title, a summary,
  context, consequences, alternatives, the numbers of the lines it cites,
  and `same_as`: the id of a known decision, or nothing. When there is
  nothing to record, a short reason for the person.
- **Prompt rules.** Every string in the request is data, never an
  instruction. An empty result is correct and frequent. A draft is valid
  only when the job can state the choice, the reason, that the people in
  the conversation settled it and did not leave it open, and the lines that
  show this.
- **Validation in code.** A cite that is not a line of the request is
  dropped. A draft with no valid cite disappears, because evidence is never
  optional. A `same_as` that is not among the known decisions, or that
  names a rejected or superseded one, is ignored and the draft counts as
  new. For a named decision exactly one draft survives, and the line where
  the person asked is always among its cites.
- **Size.** A conversation over the budget is cut to its first line and the
  newest lines that fit. The job never fails on size.

### Storage changes

- A message gains an optional external id, unique within its session: the
  Slack `ts`, a Jira comment id. The same id sent twice is one message.
  Position stays the identity of transcript turns. In a chat a deleted
  message shifts every later position, so position cannot be the identity
  there. This extends the accepted decision about identity by position.
- Decisions can be listed by the session they cite.
- The three shared tables described under "Adapters".

### Slack

**Bot.** An open route receives the Events API. It verifies the signature
over the raw bytes in constant time, with a timestamp window and a body size
limit, inserts the event into the inbox and answers at once. A worker takes
the event under a lease, resolves the person through identities and the
channel through bindings, fetches the thread, submits it and posts the
answer in the thread. The session's key is the permalink of the thread
root, so two reads of one thread name one session. Outside a thread the
conversation is the mention alone, and a request to capture is answered
with "ask me in the thread where it was decided".

**Binding.** `bind <project>` is said in the channel by a person who has an
identity and can see the project. The reply states that the lines a
decision cites become visible to the whole Staw group.

**Identity.** At first an operator command provisions identities. Then
self-service: a person with no identity gets a private reply with a
short-lived signed link that names the external account, the web app shows
a consent screen, and a signed-in person approves with a POST. The threat
this is designed against is an attacker who sends their own link to a
victim, so that the attacker's Slack account then acts as the victim. So
the screen names the external account and says what approval grants, and
opening a URL links nothing.

**Reader.** Off per binding until someone turns it on. Because of the rate
limit it is fed by events and not by history calls: the adapter subscribes
to the message events of bound channels and keeps them in a private buffer
with a retention limit. Edits and deletions apply to the buffer until a
thread is read. After that, recorded lines never change. A thread is handed
over as unprompted when it has been quiet for the settle time (four hours
by default), and again when it changes after that; the capture job then
sees the decisions already recorded from it. A loop claims a due binding
under a lease, so two replicas never read one binding. A failed
conversation is counted and does not block the binding. Unbinding stops the
reading and purges the buffer.

**Signals.** A conflict reaches the author of the new decision where that
person asked, in a direct message or in the thread, with the verdict as
buttons. It uses the existing ledger of receipts and treats the bot's
conversation with a person as a session.

### Jira

The same pipeline with another adapter. The reader polls the issues of a
bound Jira project that changed since a cursor. An issue is a conversation:
the description is its first line and each comment is a line. This needs a
new session kind. The bot is a comment that mentions Staw's account, and the
answer is a comment. A decision captured from an issue goes through the
conflict search like any other, so an issue that contradicts an accepted
decision raises a signal.

### Tools: MCP servers for the expert

Staw Expert becomes an MCP client. While a job runs, the model may call
read-only tools of configured MCP servers: read the Jira issue a thread
refers to, read a pull request, look up a page. The first users are the
capture job and `ask`.

- Configuration per server: a URL, a credential, a required allow-list of
  tool names and a timeout. A second table says which jobs may use which
  servers. With no configuration there are no tools and no calls.
- A bounded loop: the model asks, the code calls, until the model answers
  or a limit on rounds, total time or result size is reached.
- Tool results are data. Calls are logged by name, duration, size and
  outcome, never by content.
- One credential serves the whole deployment. That is acceptable for a
  single team. The hosted service needs a credential per group first.

MCP is the expert's reach and not the adapters' transport. An adapter needs
stable ids, timestamps, permalinks, events and a cursor, so it uses the
system's own API. An MCP tool answers a model in prose, has no events and
promises no stable shape.

The one exception is the last build step, a generic reader for a system
that has an MCP server and will never get an adapter. Its collect step runs
an expert job with tools ("list what changed in this place since this
cursor") and returns conversations in the intake's shape. It costs a model
call per poll and promises less than an adapter, so it is opt-in per
binding.

### Rules that hold everywhere

- A person who asked always gets an answer, including "I could not record
  this" in words that person can act on.
- Outside text is data: messages, names, titles and tool results. The
  prompts say so, the code checks every id and every cite, and a model
  produces drafts and never writes.
- A person's request runs under that person's scope. A reader writes only
  into the project of its binding.
- Logs carry ids and counts, never message text, names or tokens.
- Each contract has a fake implementation in tests from its first day, so
  it always has two implementors.

### Build order

1. The intake, the capture job and a REST entry point, with no external
   system. The caller is the asker. A dry-run mode returns the drafts and
   writes nothing. This entry point is also how any script pushes a
   conversation, for example a meeting transcript.
2. The Slack bot.
3. Self-service identity linking.
4. The Slack reader.
5. Signals through the bot.
6. The Jira adapter.
7. Tools for the expert.
8. The generic reader over MCP.

Step 7 needs only step 1. Step 6 needs the inbox of step 2 and the reader
loop of step 4. Step 8 needs steps 4 and 7.

Not in scope: the value score, moving the two existing entry points onto
the intake, a web UI for bindings, adapters other than Slack and Jira, and
installing into many Slack workspaces, which needs third-party tokens
stored encrypted.

### The test the design sets for itself

The design is right if each of these needs no change to the intake:

- GitHub pull-request discussions as a third adapter.
- The transcript read after an agent session, as an unprompted submission.
- A meeting transcript pushed through the REST entry point.

## Choices and the alternatives that lost

| Choice | Alternative | Why the choice was made |
| --- | --- | --- |
| One adapter per system, with two roles | A bot layer and a source layer | A split by trigger repeats the per-system code in both layers, and the pipeline in each. |
| Adapters compiled into the server | Separate processes that call the REST entry point; a plugin runtime | No new infrastructure, one binary to run, shared tables for the inbox, bindings and identities. The REST entry point keeps the other route open. |
| Adapters use each system's own API | MCP servers as the transport for ingestion | Ingestion needs events, cursors and stable ids, which MCP tools do not promise. |
| A fixed grammar first | A model classifies every request from the start | A named decision then works with no model and behaves the same every time. |
| One capture call drafts and judges duplicates | A separate matching step: embeddings, or a second call | The call already holds the candidates, and the code checks every id it returns. |
| Slack reading fed by events into a buffer | Polling the history | The rate limit above. |
| An unprompted capture is recorded as `proposed`, authored by the expert | Record nothing and ask in the thread (question 3 below) | `proposed` keeps unreviewed decisions apart from accepted ones. |
| A capture on request is recorded at once as `accepted` under the asker's name, and the reply shows what was recorded | Show drafts first and record on a click | It matches what an agent harness does today. |
| One place binds to one project | Bind a place to a group and let capture choose the project | It matches how a repository binds to a project. |
| Automation never supersedes | The model may replace a decision | A wrong replacement silently retires a decision that others rely on. |
| The two existing entry points stay as they are | Move them onto the intake | To keep the first step small. |

## What we already know is weak

You need not rediscover these. Tell us instead which are fatal, which are
acceptable, and how you would fix them.

1. **Noise from unprompted capture.** With no value filter, a busy channel
   can produce many `proposed` decisions, and there is no review queue
   beyond the web list. It is also undecided who receives a conflict signal
   for a decision whose author is the expert.
2. **What a conversation is in Slack.** The settle rule is defined for
   threads. Many teams decide in the main flow of a channel without
   threads, and the proposal does not say how such messages become a
   conversation.
3. **Decision-level idempotency.** Events, sessions and lines have
   deterministic keys. Decisions do not: a repeat is caught only because
   the capture job sees the decisions already recorded from the
   conversation and marks the draft `same_as`. For a named decision
   recorded with no model the proposal names no guard, although the line
   where the person asked would be a natural key. There is also no lock per
   conversation, so two requests on one thread at the same moment can
   record the same decision twice.
4. **A seam in the contract.** A submission requires a project, which comes
   from the binding. But `bind` and `help` arrive before a binding exists,
   and `bind` needs the place, which a submission does not carry. The
   proposal does not yet say where these commands are handled.
5. **Audiences differ in both directions.** Lines that a decision cites
   leave a channel, possibly a private one, and become visible to the whole
   Staw group. And the bot posts answers and summaries of decisions into a
   channel whose members may not belong to the group: guests, or another
   company in a shared channel. Binding is treated as the moment of
   consent, and nothing restricts it by channel type.
6. **Tools run with the deployment's credential, steered by untrusted
   text.** A message in a bound channel can lead the expert to read
   something that the message's author cannot see, and the result can
   surface in a decision or an answer. An allow-list of read-only tools
   limits the damage to disclosure and does not remove it.
7. **Other people's messages at rest.** The Slack buffer holds messages of
   people who asked for nothing. Recorded evidence is immutable, so a
   message deleted in Slack after capture stays in Staw. And every
   conversation that a reader hands over is sent to the configured model
   provider.
8. **One place, one project** is tight for a shared channel, such as
   #backend, that serves several projects.
9. **The fixed grammar.** "Please save this" is not a command, so it is
   answered as a question and nothing is recorded.
10. **Messages of bots.** The proposal does not say how an adapter treats
    Staw's own posts, or the posts of other bots, when it builds a
    conversation or reads a thread again.
11. **Truncation.** A long conversation is cut to its first line and its
    newest lines, so a decision made in the middle is lost with no notice.
12. **Cost.** The reader makes one model call per settled thread, and
    another each time the thread changes later. There is no budget and no
    cheap pre-filter.
13. **Three write paths.** The intake is the only writer for adapters, but
    the two existing entry points stay beside it. So the shared rules
    (evidence, author, duplicate handling, detection) hold in three places
    by convention.
14. **The hosted service.** One workspace per deployment, with the token in
    configuration, is all that is designed. The only provision for more is
    a workspace column in the keys of bindings and identities.

## What we ask of you

Review the approach, not the wording. The questions we most want answered:

1. **The boundary.** Is "adapters state facts, the intake chooses the path
   and is the only writer" the right cut? Is any responsibility on the
   wrong side of it: interpreting the ask, resolving the identity and the
   binding, fetching the thread, rendering the answer? Would you separate
   the bot and the reader after all?
2. **Adapters inside the server.** Would you compile adapters into the
   server, or run them as separate workers against the REST entry point?
   What does each choice cost when the tenth system arrives, and for the
   hosted service?
3. **Authority.** Is `accepted` right for a decision that a model drafted
   and a person only requested? Is `proposed` under the expert's name right
   when nobody asked? Or should the reader record nothing, and the bot ask
   in the thread "it looks like you decided X, record it?" We have not
   evaluated that alternative.
4. **Duplicates and idempotency.** Is one model call that both drafts and
   judges duplicates sound? Which failure hurts more here, a false "same"
   or a false "new"? What deterministic guards would you add?
5. **The reader.** Is "quiet for N hours, then read, then read again on
   change" the right trigger? What would you use as the unit of
   conversation in Slack and in Jira?
6. **Security and privacy.** Where is the design exposed: the identity
   link, the open webhook route, prompt injection through conversations and
   tool results, the tool credential, the two audience mismatches,
   retention? What is the minimum you would require before the reader
   ships, and before the tools ship?
7. **Storage.** Is a Postgres inbox with leases enough as the only queue?
   Is the external message id the right extension of identity by position?
   Does the workspace column make a later move to many workspaces cheap, or
   does that move reach deeper?
8. **MCP.** Is "the expert's reach, not the adapters' transport" the right
   place for MCP? Is the generic reader over MCP worth building at all?
9. **Order.** Is the first build step the right one? What would you cut,
   merge or move, and what is the smallest version that proves the design?
10. **What is missing.** Failure modes, operational concerns or product
    consequences that this text does not mention at all.

The form of answer that helps us most:

- **A verdict first**, in a few sentences: build it as proposed, build it
  with changes, or redraw it.
- **Findings, most serious first.** For each: a concrete scenario (who does
  what, and what the system then does), why the design allows it, what you
  would change, and how confident you are.
- **Answers to the ten questions**, short where a finding already covers
  one.
- **What you could not judge** from this text, and what you assumed.

Please do not write code, and do not rewrite the whole design unless you
think the boundary is wrong. In that case, describe the boundary you would
draw and what it buys. If you disagree with one of the team's decisions
under "Constraints", say so as well: half of them are still proposals.
