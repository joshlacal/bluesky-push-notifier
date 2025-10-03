#!/bin/bash

# Fix challenge parameters for async wrapper calls (need .clone() to convert &String to String)
sed -i \
  -e 's/\&req\.proof\.challenge,/req.proof.challenge.clone(),/g' \
  -e 's/\&query\.proof\.challenge,/query.proof.challenge.clone(),/g' \
  api.rs

# Fix public_key parameters for async wrapper calls (remove & to convert &Vec<u8> to Vec<u8>)  
sed -i 's/\&public_key,$/public_key.clone(),/g' api.rs
