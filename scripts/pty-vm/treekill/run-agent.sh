#!/usr/bin/env bash
# s9k: Claude Code or omp in a tendr PTY; $1 = name, $2 = kill|force|timeout,
# $3 = settle seconds, $4 = timeout seconds (timeout mode), $5 = prompt|noprompt, $6 = claude|omp
name=$1 mode=$2 settle=$3 tmo=$4 prompt=$5 agent=$6
extra=()
[ "$mode" = timeout ] && extra=(--timeout "$tmo")
text='Do two things with your bash tool. First run: tail -f /dev/null  as a background task (use your run-in-background option). Second run: (setsid tail -f /dev/null >/dev/null 2>&1 &)  as a normal command. Then reply with the single word: ok'
if [ "$agent" = claude ]; then
  cmd=(claude --allowedTools=Bash)
else
  cmd=(env ANTHROPIC_API_KEY=exe-gateway ANTHROPIC_BASE_URL=https://llm.int.exe.xyz
       "$HOME/.local/bin/omp" --profile s9k --no-session --no-title --auto-approve
       --model anthropic/claude-haiku-4-5)
fi
[ "$prompt" = prompt ] && cmd+=("$text")
cd "$(dirname "$0")" && ./treekill.py "$name" "$mode" "$settle" "${extra[@]}" --cwd "$HOME/work" -- "${cmd[@]}"
