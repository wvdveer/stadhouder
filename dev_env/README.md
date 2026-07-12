# dev_env

Per-developer, non-committed configuration. Each `*.conf.example` file here
documents one config file; copy it without the `.example` suffix and edit.
The real `*.conf` files are gitignored.

| File                 | Used by             | Purpose                                                |
|----------------------|----------------------|--------------------------------------------------------|
| `keyscarf.conf`      | `fetch-keyscarf.sh` / `stage-keyscarf.sh` | Override where the KeyScarf release comes from |
| `keyscarf-db.conf`   | `stage-keyscarf.sh` | Pre-seeds KeyScarf's DB connection (`authsite.conf`) so its setup wizard skips straight to schema install |
