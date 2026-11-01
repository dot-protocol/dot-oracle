# Troubleshooting

Start with `ox doctor`. It names the failing layer.

| Symptom | Cause | Fix |
|---|---|---|
| `key file … is readable by group or others` | key file mode is too open | `chmod 600 <file>` |
| `key is registered ✗` | the registry holds a different key for this seat | owner runs `oracle seat revoke <h>` then `oracle seat add <h> --pubkey <your pubkey>` |
| `server refused (401): timestamp outside the accepted window` | clock skew over 5 minutes | fix the machine clock (NTP) |
| `server refused (401): signature does not verify` | wrong key for this handle, or the envelope was altered in transit | `ox whoami` — does the local pubkey match the registry? |
| `server refused (403): token belongs to 'a' but the envelope is authored by 'b'` | token and handle disagree in `config.json` | re-run `ox setup` with the right pair |
| exit code `3`, "queued" | server unreachable; the write is safe in `~/.oracle/outbox/` | bring the server back, `ox flush` |
| `*.rejected` files in the outbox | the server answered and refused that write; retrying will not help | read the reason printed by `ox flush`, fix, re-send by hand |
| `database is locked` under heavy load | another process holds a long write transaction | only one `oracle serve` per database; backups use `.backup`, not `cp` |

## Recovering access

- **Lost a seat key:** the owner revokes and re-adds the seat. History signed with the old
  key stays valid and attributable.
- **Lost a token:** `oracle token new <handle>` writes a new one to `keys/<handle>.token`
  (mode 0600; `--token-file <path>` to choose; an existing file is never overwritten) and
  `ox setup --token-file` reads it. Old tokens keep working until the seat is revoked.
- **A token was printed or logged:** `oracle seat revoke <handle>` (every token and key),
  then add the seat again with a new key and set it up from the token file.
- **Lost the owner key:** there is no recovery by design. Restore it from backup.
