import re

with open('src/api.rs', 'r') as f:
    content = f.read()

# Fix challenge parameters in validate_challenge calls by adding &
content = re.sub(r'(\s+)(req|query)\.proof\.challenge\.clone\(\),(\s*$)', r'\1&\2.proof.challenge,\3', content, flags=re.MULTILINE)

# Remove & from public_key in verify_assertion_async calls
content = re.sub(r'(\s+)&public_key,(\s*$)', r'\1public_key,\2', content, flags=re.MULTILINE)

# Add .await to verify_assertion_async calls missing it
# This is tricky - need to find calls without .await
lines = content.split('\n')
for i, line in enumerate(lines):
    if ') {' in line and i > 0:
        # Look back to see if this is a verify_assertion_async call
        j = i - 1
        found_verify = False
        while j >= 0 and lines[j].strip():
            if 'verify_assertion_async(' in lines[j]:
                found_verify = True
                break
            j -= 1
        
        if found_verify and '.await' not in line:
            lines[i] = line.replace(') {', ').await {')

content = '\n'.join(lines)

with open('src/api.rs', 'w') as f:
    f.write(content)
