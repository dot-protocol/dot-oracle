# Security

Oracle's security model is in [docs/DESIGN.md](docs/DESIGN.md) § Trust. In short: every write
is an Ed25519-signed envelope checked against a key registry; tokens are bound to a seat and
stored hashed; key changes must themselves be signed; a bad signature is a rejected write.

## Reporting a vulnerability

Please report privately via GitHub's **Report a vulnerability** (Security tab) rather than a
public issue. Include a reproduction if you can — `scripts/e2e.sh` is a good harness to
extend. You will get an acknowledgement within 72 hours.

## What is in scope

Forging or replaying a write; writing as another seat; changing a key without authority;
reading data without a token; recovering a token or key from the database or logs; denial of
service through a single unauthenticated request.

## Operational notes

- The owner key file is the root of trust. Keep an offline backup; never paste it anywhere.
- Oracle serves plain HTTP on loopback. Put TLS in front of it before exposing it
  (`docs/running.md`).
- Run the server as an unprivileged user.
