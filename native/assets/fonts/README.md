# Interface font

Inter 4.001 (SIL Open Font License 1.1, see `OFL.txt`), the same typeface the
product portal ships, so the native application and the portal render alike on
every system.

The files are static instances (400, 500, 600) of the Latin and Latin Extended
subsets from the npm package `@fontsource-variable/inter` 5.3.0. The Latin
Extended subset carries the Polish letters; load both subsets of a weight into
one font family, Latin first.

Regenerate with fontTools (`pip install fonttools brotli`):

    python build-fonts.py <path to node_modules/@fontsource-variable/inter> .
