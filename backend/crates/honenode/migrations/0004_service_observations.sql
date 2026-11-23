ALTER TABLE services ADD COLUMN observed_at TEXT;

-- Earlier releases copied command acknowledgements into observed_state, without evidence.
UPDATE services SET observed_state = NULL;
