import re

with open('src/api.rs', 'r') as f:
    content = f.read()

# Fix all remaining verify_assertion calls 
content = re.sub(
    r'state\.app_attest\.verify_assertion\(',
    r'verify_assertion_async(\n        &state.app_attest,',
    content
)

# Fix all the parameter types based on the successful calls
# For verify_assertion_async calls, we need .clone() for String/Vec params
content = re.sub(r'&req\.proof\.assertion,', r'req.proof.assertion.clone(),', content)
content = re.sub(r'&req\.proof\.client_data,', r'req.proof.client_data.clone(),', content) 
content = re.sub(r'&req\.proof\.challenge,', r'req.proof.challenge.clone(),', content)
content = re.sub(r'&query\.proof\.assertion,', r'query.proof.assertion.clone(),', content)
content = re.sub(r'&query\.proof\.client_data,', r'query.proof.client_data.clone(),', content)
content = re.sub(r'&query\.proof\.challenge,', r'query.proof.challenge.clone(),', content)
content = re.sub(r'&public_key,', r'public_key.clone(),', content)

# For verify_assertion_async, device.app_attest_challenge should use .clone(), not .as_deref()
# But only in verify_assertion_async calls, not validate_challenge calls
lines = content.split('\n')
in_verify_assertion = False
for i, line in enumerate(lines):
    if 'verify_assertion_async(' in line:
        in_verify_assertion = True
    elif in_verify_assertion and 'device.app_attest_challenge.as_deref()' in line:
        lines[i] = line.replace('device.app_attest_challenge.as_deref()', 'device.app_attest_challenge.clone()')
        in_verify_assertion = False
    elif in_verify_assertion and (').await {' in line or ') {' in line):
        in_verify_assertion = False

content = '\n'.join(lines)

# Add .await to the verify_assertion_async calls that are missing it
# Look for patterns like:
# verify_assertion_async(
#     ...
# ) {
# and change to:
# ) {
content = re.sub(
    r'(\s+verify_assertion_async\([^)]*?\n(?:[^)]*?\n)*?\s+)\) \{',
    r'\1).await {',
    content,
    flags=re.MULTILINE | re.DOTALL
)

# Fix the verify_assertion_async function to use &public_key instead of public_key.clone()
content = re.sub(r'public_key\.clone\(\),(\s*//.*?)?(\s*previous_counter,)', r'&public_key,\1\2', content)

with open('src/api.rs', 'w') as f:
    f.write(content)
