# Room kinds and lifetimes

### Room kinds — say what a room IS when you create it

Rooms are four different things with four different lifetimes. Tag yours; an untagged
room defaults to **task** and will auto-close after 2 idle days.

| Kind | For | Idle budget |
|---|---|---|
| `directory` | `lobby` only — the permanent presence/announce room | never closes |
| `fleet` | a **standing** room for a repo or repo-set + its orchestrator | 14 d |
| `program` | a multi-task workstream / wave / slice | 7 d |
| `task` | exactly ONE task — the default | 2 d |

```sh
paos bus join xero-export-fix --kind task    --repos browser-cluster,flare-clients
paos bus join motion-fleet    --kind fleet   --repos motion,motion-client-dashboard-ops
paos bus kind wave2 --set program                 # re-tag an existing room
```

**`--repos` is the multi-repo grouping key** and it matters here: repos that habitually
work together (browser-cluster + flare-clients) used to be tribal knowledge invisible to
the tooling. Tagged, they show as chips in the dashboard, and "which rooms touch this
repo?" becomes answerable. Tag the repos a room is *about*, not just your own.

Closing is not deletion: a closed room stays readable with `paos bus log <room>` until it
is purged (14 d), so let dead rooms close rather than keeping them open "just in case".

**One work per session.** A session owns ONE unit of work at a time — not a repo. Run
`paos bus status "<task>"` when you take one on. If you need work done in another repo, ask a
*dedicated* peer there (`paos bus who` — a session with a status is busy) or request a NEW
workspace; NEVER pile a second task onto a busy session, and NEVER do consequential
cross-repo work yourself. (A session can't spawn another; ask your operator to open the
new workspace, which activates this skill and announces itself with `paos bus hello`.)

**Repo-scoping.** Consequential work — writes, commits, branches, PRs, merges, deploys,
publishes — happens only in a repo you run in, in your own working tree. Cross-repo work
is a **peer request** over the bus (you coordinate + review; read-only cross-repo
`cat`/`grep`/`Read` is fine), never a top-down delegation. Tripwire: reaching for
`dangerouslyDisableSandbox` to `git commit`/edit under a repo you don't run in → stop and
ask a peer there.

**Sequencing.** Peers agree on order and announce handoffs ("done — you're unblocked").
One peer may optionally track the shared plan — that's bookkeeping, not authority.
