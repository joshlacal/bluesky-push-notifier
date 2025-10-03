import re

with open('src/api.rs', 'r') as f:
    content = f.read()

# Fix challenge parameters in verify_assertion_async calls (should be .clone())
content = re.sub(r'(\s+)&(req|query)\.proof\.challenge,(\s*$)', r'\1\2.proof.challenge.clone(),\3', content, flags=re.MULTILINE)

# Fix the verify_assertion function call to use &public_key
content = re.sub(r'(\s+)public_key,(\s*//.*?)?(\s*previous_counter,)', r'\1&public_key,\2\3', content)

with open('src/api.rs', 'w') as f:
    f.write(content)
