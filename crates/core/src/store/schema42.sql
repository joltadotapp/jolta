-- The size a draft's pasted images go out at: small, medium, large or actual.
ALTER TABLE draft ADD COLUMN image_size TEXT NOT NULL DEFAULT 'actual';
