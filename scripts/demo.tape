# VHS demo recording script for Athena
# Prerequisites: install VHS (https://github.com/charmbracelet/vhs)
# Run: vhs scripts/demo.tape
# Output: docs/demo.gif

Output docs/demo.gif

Set FontSize 14
Set Width 1200
Set Height 620
Set Theme "Dracula"
Set Padding 20

# Set up demo alias invisibly
Hide
Type "alias athena='bash scripts/demo-wrapper.sh'"
Enter
Sleep 300ms
Show
Ctrl+L
Sleep 300ms

# Act 1: Show the agent roster
Type "# Ghost agents configured for this workspace"
Enter
Sleep 200ms
Type "athena ghosts"
Enter
Sleep 6s

# Separator
Type "# Dispatching an autonomous task..."
Enter
Sleep 400ms

# Act 2: Dispatch — wrapper prints output line-by-line with delays (~12s total)
Type `athena dispatch --goal "Add per-IP rate limiting to the login endpoint"`
Enter
Sleep 15s

# Act 3: KPI — show success rate after task completes
Type "# Mission KPIs"
Enter
Sleep 200ms
Type "athena kpi status"
Enter
Sleep 4s

Sleep 2s
