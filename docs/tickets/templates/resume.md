# Resume prompt template (continue a partial ticket)

---

You are resuming ticket {TICKET_ID} in the worktree at {WORKTREE_PATH},
branch `ticket/{TICKET_ID}-{SLUG}`. A previous agent worked on it and
stopped. Their state:

- Read `HANDOFF.md` at the worktree root FIRST - it lists completed work
  (with commits), remaining steps, blockers, and evidence so far.
- `git log --oneline main..HEAD` shows what landed.

Your ticket spec and all rules are the same as the original implementer's:
read `docs/tickets/templates/implementer.md` and apply it fully, with the
ticket section below.

{TICKET_SECTION_VERBATIM}

Specific to resuming:
1. Verify the HANDOFF's claims cheaply before building on them (run the
   listed gate commands; if evidence does not reproduce, treat that item as
   REMAINING, note the discrepancy).
2. If HANDOFF.md lists BLOCKERS/QUESTIONS: check whether
   `docs/tickets/execution-plan.md` or the ticket file records an answer
   (the orchestrator writes decisions there). Unanswered -> you inherit the
   STOP; update HANDOFF.md and stop too.
3. Rewrite HANDOFF.md on your own stop - it must always reflect the LATEST
   state.
