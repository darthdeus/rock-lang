# rock.nvim development

The root `AGENTS.md` applies here. Plugin setup, commands, and test instructions
are in [README.md](README.md).

Modules update the observable store through `state.lua`; the panel and
statusline subscribe to its snapshots. Request timings deliberately do not
notify on every request, avoiding panel redraws on the hot path.
