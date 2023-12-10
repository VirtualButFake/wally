# Lune-wally changelog
## v1.1.0
- The backend will now combine server & dev dependencies into shared dependencies (might make it throw an error instead)
- The CLI tool will now ignore dependencies under ``server-dependencies`` & ``dev-dependencies`` and warn the user about it
- The CLI tool will now check whether the package contains a direct ``init.lua(u)`` file, or whether this is inside of a ``src`` folder, and if so modify all requires to point to that folder instead. 