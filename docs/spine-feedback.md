# Spine feedback

SpineCodex uses a separate `/feedback` flow when the current thread has
SpineJIT, SpineTrim, or SpineSpawn enabled. If none of those features is
enabled, `/feedback` keeps the Base Codex category, log-consent, and upload
flow unchanged.

The Spine feedback form accepts:

- An optional note of at most 8,192 UTF-8 bytes.
- Zero to three screenshots.
- An explicit confirmation before anything is sent.

## Rollout debug attachment

Every Spine feedback report contains one `rollout-debug.jsonl.gz`. It covers
the current thread and every recursively spawned descendant known when the
request starts. There is no child-count limit. A child created after that
snapshot belongs to a later report.

The attachment retains diagnostic structure:

- record and thread order;
- Spine control argument shape and typed outcome;
- compact boundaries and token usage;
- package-local IDs that preserve equality only inside the attachment.

It removes conversation text, model output, tool bodies, summaries, memories,
raw thread and call IDs, paths, and URLs. Unknown, malformed, or over-8 MiB
source lines remain in their original positions as redacted placeholders.
Raw rollouts and generic logs are not attached.

The redactor also has fixed budgets for package-local ID mappings and pending
tool calls. If either budget is exhausted, attachment generation fails closed;
SpineCodex does not send a partial debug package.

This file is observational diagnostic evidence. It cannot replay a session,
restore Spine state, or recover the removed content.

## Screenshots

The form accepts PNG, JPEG, and static WebP images. Animated WebP and other
formats are rejected. Image pixels are decoded and encoded as new PNG files
in both the client and the app server, so source metadata, paths, and
filenames are not sent.

Screenshot pixels are **not redacted**. Source code, terminal text, paths,
account details, and secrets visible in a screenshot remain visible in the
uploaded pixels. Review every screenshot before confirming the report.

Limits:

- At most three screenshots.
- At most 8,192 pixels on either side and 16 megapixels per image.
- At most 5 MiB per encoded PNG and 10 MiB for all screenshots.
- At most 20 MiB for all report attachments.
- At most 20 MiB for each encoded image source read by the client.

## Destination and report ID

After confirmation, SpineCodex sends the report directly to the
`spinecodex / spine-codex-feedback` Sentry project in Sentry's EU data region.
The application reports success only after the ingest endpoint returns HTTP
2xx, then displays the 32-character Sentry report ID.

An HTTP 2xx response means that Sentry accepted the inbound envelope. It does
not guarantee that indexing has completed or that the event or its attachments
will be retained for a particular duration. Retention follows the Sentry
organization and project policy in effect at the time of submission.
