# Raycast and SuperCmd

Type a context name directly in your launcher's main search and press Return on its Sugarglider Contexts result.

## Set up once

1. Run Sugarglider with Contexts enabled.
2. In Raycast, use **Add Script Directory** in Script Commands settings and select `~/.config/raycast/script-commands/sugarglider-contexts`. SuperCmd 1.0.26 scans this folder automatically.
3. Open the launcher and type the name of a context.

To print the folder's absolute path:

```bash
sugarglider context launcher-path
```

Sugarglider updates the commands when you create, rename, or delete a context. Everything is always listed while Contexts is enabled. Unsorted appears when Sugarglider lists it in the context switcher. Disabling Contexts removes the generated commands unless a filesystem or ownership conflict blocks the update.

The commands use stable context IDs, so renaming keeps launcher shortcuts attached to the same context. A deleted context returns an error instead of matching another name. Replacing or deleting the saved context database can reuse IDs; recreate launcher shortcuts after a database reset. The script calls the CLI beside the running server, including for development builds. It does not require `/usr/local/bin/sugarglider`.

Leave generated files unchanged. Sugarglider preserves unrelated files and refuses to overwrite an edited command. A conflict blocks the whole update, including removals. Failures appear in the server log, and the worker retries with delays up to 60 seconds. If Sugarglider is stopped, an indexed command reports an error; it never starts the manager.

Raycast documents automatic reindexing of metadata edits; discovery of additions and deletions remains physical QA. SuperCmd 1.0.26 caches script discovery for 12 seconds; a changed result may need time to refresh. Physical launcher verification is still pending.

## Optional query command

`switch-context.sh` provides a separate **Switch Context** command that takes a name, partial name, or number. Import this `contrib/raycast` folder to use it as a fallback command. This optional script expects `/usr/local/bin/sugarglider`, where `make install` puts the CLI.
