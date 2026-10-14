prefix := env_var_or_default("HOME", "/root") + "/.local"
share := prefix + "/share"
app_id := "io.github.mozartdiniz.Leporello"

# Leporello — GTK client for iCloud Reminders

# Run the app (debug)
run:
    cargo run

debug:
    RUST_LOG=leporello=debug,remindkit=debug cargo run

test:
    cargo test

# Check the live account from the CLI (schema survey; see remindkit/examples/probe.rs)
probe *args:
    cargo run -p remindkit --example probe -- {{args}}

# Made-up data in the app cache, for UI work without an account (sign out after!)
fixture:
    cargo run -p remindkit --example fixture -- "${XDG_CACHE_HOME:-$HOME/.cache}/leporello/store.json"

# Install into ~/.local, and start in the background at login so alerts fire
install:
    cargo install --path . --root {{prefix}}
    install -Dm644 data/{{app_id}}.desktop -t {{share}}/applications/
    install -Dm644 data/{{app_id}}-autostart.desktop "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/{{app_id}}.desktop"
    -update-desktop-database {{share}}/applications

uninstall:
    -cargo uninstall leporello --root {{prefix}}
    rm -f {{share}}/applications/{{app_id}}.desktop
    rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/{{app_id}}.desktop"
    -update-desktop-database {{share}}/applications
