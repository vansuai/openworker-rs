import subprocess, sys
r = subprocess.run([sys.executable, "scripts/daily_ai_trending.py"], capture_output=True, text=True)
print("RC:", r.returncode)
print("STDOUT:", r.stdout)
print("STDERR:", r.stderr)
