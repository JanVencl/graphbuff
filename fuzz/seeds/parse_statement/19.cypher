CREATE VECTOR INDEX docs IF NOT EXISTS FOR ()-[r:NEAR]-() ON (r.v) WITH [r.since, r.`the kind`] OPTIONS {indexConfig: {`vector.dimensions`: 3}}
