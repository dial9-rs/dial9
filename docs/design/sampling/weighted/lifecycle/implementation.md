Implemented the terminal recorder fix in shared checkout, without committing or pushing.

Files changed:
- dial9-core/src/shared_state.rs: the existing permanent disabled state is now independent of the pipeline feature; monotone fetch_max preserves terminal shutdown.
- dial9-core/src/handle.rs: weak `disable_permanently_callback` invokes the permanent state transition; tests preserve reversible pause/resume, refuse both public enable APIs afterward, verify normal flushing/shutdown, and verify weak lifetime. A Shuttle scenario races the actual callback against pause/resume.
- dial9-core/src/recording.rs: enable documentation distinguishes reversible pause from permanent disable.
- dial9-sampling/src/lib.rs: only register_recorder's callback call changed to the permanent callback (other concurrent edits belong to root).
- dial9-sampling/src/tests.rs: the existing actual roster-trim regression now attempts both enable APIs on trimmed recorders.

Validation completed:
- cargo fmt -p dial9-core -- --check: passed.
- cargo nextest run -p dial9-core --no-default-features -E 'test(permanent_disable)': 2 passed. This specifically proves the callback works without pipeline.
- env RUSTFLAGS='--cfg shuttle' cargo test -p dial9-core --lib --features _shuttle -- shuttle_permanent_disable_races_pause_resume: PCT and determinism passed (2 checks).
- cargo nextest run -p dial9-sampling --features dial9 -E 'test(roster_trim_disables_registered_recorders_without_application_tasks)': passed, including real terminal trim then both enable APIs.
- cargo clippy -p dial9-core --all-targets --all-features: passed.

Unchanged diagnostics: Shuttle emitted the existing three missing-Debug warnings in primitives.rs. Full workspace stress and complete Shuttle verification belong to root's final combined change validation.
