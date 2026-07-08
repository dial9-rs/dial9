# Implementer prompt template

Fill the placeholders, spawn one agent per ticket in a worktree.

---

You are implementing ONE ticket of the dial9-viewer UI migration in the repo
at {REPO_PATH} (your worktree, branch `ticket/{TICKET_ID}-{SLUG}`).

Your ticket (the full spec - its links are your context budget; read them,
one hop):

{TICKET_SECTION_VERBATIM}

Shared conventions binding you (read these files first):
- `docs/tickets/chunk-1-foundation.md` header: conventions + SHARED DECISIONS
  block (routing, budgets, verdict mapping, ledger, DoD notation, test-suite
  reality, aggregation recipe, colocation).
- AGENTS.md (repo rules; the frozen-core list is absolute unless your ticket
  says otherwise).

Hard rules:
1. STOP-ON-GATE: if you hit an ambiguity with two materially different
   readings, a missing decision, or a conflict between your ticket and the
   code - STOP. Write `HANDOFF.md` (see rule 4) describing the question and
   options. Do NOT pick a "working assumption".
2. Commit at every coherent step, descriptive messages, conventional format.
   Never leave >30 min of work uncommitted.
3. DoD discipline: every `check:` item must actually pass before you declare
   done - run the commands, paste the evidence into `HANDOFF.md`. `review:`
   items: list them explicitly for the human reviewer.
4. On ANY stop (done, blocked, or budget): write `HANDOFF.md` at the
   worktree root: STATUS (done|blocked|partial), COMPLETED (with commit
   shas), REMAINING (concrete next steps), BLOCKERS/QUESTIONS, EVIDENCE
   (gate outputs). This file is how work survives you.
5. Scope fence: your ticket's Out-of-scope is absolute. Unrelated bugs you
   find: note in HANDOFF.md, do not fix.
6. Do not push, do not open PRs, do not touch GitHub - branch + HANDOFF.md
   is your deliverable.
