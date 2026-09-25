# S8 paused-worktree resume (partial)

Base: `358761cd7a45ba72a5aa6d14a31792d39190eb19`. The six-file S8 worktree diff was preserved; this checkpoint only splits `destination.rs` mechanically to satisfy the 800-line limit and retains the actual focused transcripts. No S9 work or production-data access occurred.

RED transcripts captured before their production edits:

- `issue-1006-s8-minimal-red.log`: 9 assertion failures; three other assertions passed already.
- `issue-1006-s8-heartbeat-red.log`: 1 conflicting-target assertion failure.
- `issue-1006-s8-web-red.log`: 1 missing-Web-instance assertion failure.
- `issue-1006-s8-dest-schema-red.log`: 1 required destination-column assertion failure.
- `issue-1006-s8-output-key-red.log`: 1 verification-output external-identity assertion failure.

Passing focused transcripts after the preserved WIP and mechanical split:

- `issue-1006-s8-resume-unit-green.log`: 12 focused unit assertions pass.
- `issue-1006-s8-resume-heartbeat-green.log`: 1 heartbeat assertion passes.
- `issue-1006-s8-resume-integration-green.log`: 3 S8 integration assertions pass.

The isolated unit tests for a missing Web instance and reading an existing Web author/role are *not* proof that complete Web identity cardinality, author/role mapping, or Web credential migration works. `instance_semantic` still expects Discord/Nostr fields (`subject_id`, `config_b64`, `addresses`) for Web, while `current_semantic` reads Web's `author_id`/bearer role; therefore a populated Web instance cannot pass the normal expected-row comparison. The production mapping edits for this seam lack their own pre-edit RED and remain incomplete. No claim of S8 stage approval or complete Web GREEN is made here. A later Web mapping fix requires a forward RED at its actual mapping seam.

Credential gaps at that same normal migration seam: `apply_instance` requires a non-NULL existing envelope for every existing instance and does not install a selected file/legacy credential when that envelope is absent; existing Web identity/credential mapping is not proven end-to-end. The destination report also retains a source descriptor that can include an agent identifier; output redaction needs its own production-seam assertion before S8 can claim a fully redacted verification artifact. None of these gaps was patched in this checkpoint.
