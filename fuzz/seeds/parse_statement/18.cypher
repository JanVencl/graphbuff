MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR $q WHERE d.tenant = 5 AND 2020 <= d.year < 2024 AND NOT d.open LIMIT 10) SCORE AS score WHERE score > 0.5 RETURN d.text, score
