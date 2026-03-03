# VHS demo recording script for Athena
# Prerequisites: install VHS (https://github.com/charmbracelet/vhs)
# Run: vhs scripts/demo.tape
# Output: docs/demo.gif
#
# The demo uses --skip-llm / ghosts (no network required) so it runs
# deterministically in CI without any API keys.

Output docs/demo.gif

Set FontSize 14
Set Width 1200
Set Height 600
Set Theme "Dracula"

# Brief pause before starting
Sleep 500ms

Type "# List configured ghost agents (no LLM required)"
Enter
Sleep 300ms

Type "ATHENA_DISABLE_HOME_PROFILES=1 cargo run --quiet -- ghosts"
Enter
Sleep 8s

Type "# Run the doctor health check (skips LLM connectivity)"
Enter
Sleep 300ms

Type "cargo run --quiet -- doctor --skip-llm"
Enter
Sleep 10s

Type "# Done! See docs/ for full documentation."
Enter
Sleep 2s
