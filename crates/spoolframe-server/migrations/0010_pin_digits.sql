-- How many digits each PIN has, for clients to show ("Set · 4 digits")
-- and to size a PIN pad. The hash cannot tell, so a PIN set before this
-- has none (NULL) until it is set again: never guessed.
ALTER TABLE pin_credentials ADD COLUMN digits INTEGER CHECK (digits BETWEEN 4 AND 8);
