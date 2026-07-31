---
description: Post an announcement to a crew bus channel
argument-hint: "[#channel] message"
---

Post an announcement to the crew bus.

Input: $ARGUMENTS

Steps:
1. If the input starts with `#name`, use that as the channel; otherwise use the channel `general` (create it with `create_channel` if `list_channels` shows it does not exist).
2. Post the rest of the input as the message body with `post_message`, prefixed with a short subject line if the message is longer than a sentence or two. Keep the wording as I gave it — do not embellish.
3. If the message announces something that blocks others (deploy, migration, breaking change), also take the matching `acquire_lock` / task action if I explicitly asked for it in the message; otherwise just post.

Confirm with the channel name and message id once posted.
