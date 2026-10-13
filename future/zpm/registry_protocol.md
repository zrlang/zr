# Registry Protocol (zpm)

## Endpoints
| Method | Path                                  | Purpose              |
|--------|---------------------------------------|----------------------|
| GET    | `/api/v1/crates`                      | List all crates      |
| GET    | `/api/v1/crates/:name`                | Crate metadata       |
| GET    | `/api/v1/crates/:name/:version`       | Version metadata     |
| GET    | `/api/v1/crates/:name/:version/deps`  | Dependency tree      |
| GET    | `/api/v1/crates/:name/:version/download` | Download tarball  |
| PUT    | `/api/v1/crates/:name/:version`       | Publish (auth req.)  |

## Authentication
Token-based via `Authorization: Bearer <token>`. Tokens generated on the
registry website and stored in `~/.zpm/credentials`.

## Content types
- Metadata: `application/json`
- Artifacts: `application/x-tar+gzip`

## Limits
- Max artifact size: 50 MiB.
- Rate limit: 60 req/min for anonymous, 600 req/min for authenticated.
