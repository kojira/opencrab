# opencrab-discord

Discord webhook notification adapters used by the server.

Discord ingress and reply delivery are not implemented in this crate. They run exclusively in the external `opencrab-discord-gateway` binary over extgate V3. Bot credentials are passed only to that supervised child process.
