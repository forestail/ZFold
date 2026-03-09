HPACK(1)

NAME

    hpack - small-file solid archive CLI for directory snapshots and backups

SYNOPSIS

    hpack <COMMAND> [OPTIONS]

DESCRIPTION

    hpack recursively scans a directory, groups files into solid chunks, compresses each chunk
    with zstd, and writes a single archive file.

    Archives may also be encrypted with a password. Encryption covers:
        chunk data
        optional dictionary blob
        binary index

    The header and footer remain plaintext so the tool can detect archive version, flags, and
    metadata layout before asking for a password.

    The archive format is version 1. It uses:
        fixed-size header
        compressed chunk data
        optional dedicated dictionary blob
        binary index
        fixed-size footer

    extract reads only the required chunks, but decompression granularity is chunk-level rather
    than file-level.

COMMANDS

    pack
        Pack a directory into an archive.

    list
        List archive metadata and all file entries.

    extract
        Extract all files or a filtered subset.

    verify
        Verify archive structure and per-file CRC32.

    train-dict
        Train a zstd dictionary from one or more input directories.

PACK

    SYNOPSIS
        hpack pack <INPUT_DIR> -o <ARCHIVE>
            [--chunk-size <BYTES>]
            [--level <LEVEL>]
            [--threads <N>]
            [--dict <DICT_FILE>]
            [--password <PASSWORD>]
            [--password-env <VAR>]
            [--password-file <PATH>]
            [--password-prompt]
            [--exclude <GLOB>]...

    OPTIONS
        <INPUT_DIR>
            Input directory to archive.

        -o, --output <ARCHIVE>
            Output archive path.

        --chunk-size <BYTES>
            Target chunk size in bytes.
            Default: 16777216 (16 MiB)

            This is a target, not a hard cap. A single large file may produce a chunk larger
            than this value.

        --level <LEVEL>
            zstd compression level.
            Default: 5

        --threads <N>
            Number of rayon worker threads.
            If omitted, rayon uses its default thread count.

        --dict <DICT_FILE>
            Use a pre-trained zstd dictionary file.
            If specified, the dictionary bytes are embedded into the archive as a dedicated blob.

        --password <PASSWORD>
            Read the archive password directly from the command line.
            Supplying this option enables archive encryption.
            Mutually exclusive with --password-env, --password-file, and --password-prompt.

        --password-env <VAR>
            Read the archive password from the named environment variable.
            Supplying this option enables archive encryption.
            Mutually exclusive with --password, --password-file, and --password-prompt.

        --password-file <PATH>
            Read the archive password from a file.
            Trailing CR/LF is trimmed.
            Supplying this option enables archive encryption.
            Mutually exclusive with --password, --password-env, and --password-prompt.

        --password-prompt
            Prompt for the archive password on the terminal.
            Supplying this option enables archive encryption.
            Mutually exclusive with --password, --password-env, and --password-file.

        --exclude <GLOB>
            Exclude files matching the glob against archive-relative paths.
            May be specified multiple times.

    NOTES
        Using --password exposes the password in shell history and process listings.

LIST

    SYNOPSIS
        hpack list <ARCHIVE>
            [--password <PASSWORD>]
            [--password-env <VAR>]
            [--password-file <PATH>]

    OPTIONS
        --password <PASSWORD>
            Read the archive password directly from the command line.

        --password-env <VAR>
            Read the archive password from the named environment variable.

        --password-file <PATH>
            Read the archive password from a file.

    OUTPUT
        Prints:
            archive
            version
            encrypted
            size
            files
            chunks
            chunk_size
            dictionary
            dictionary_size
            index_size

        Then prints one line per file as:
            <ORIGINAL_SIZE>\t<CHUNK_ID>\t<PATH>

    NOTES
        If the archive is encrypted and no password source is specified, hpack prompts for the
        password before reading the index.

EXTRACT

    SYNOPSIS
        hpack extract <ARCHIVE> -d <OUT_DIR>
            [--only <PATH>]
            [--prefix <PREFIX>]
            [--password <PASSWORD>]
            [--password-env <VAR>]
            [--password-file <PATH>]

    OPTIONS
        <ARCHIVE>
            Input archive path.

        -d, --out-dir <OUT_DIR>
            Output directory for extracted files.

        --only <PATH>
            Extract exactly one archive-relative path.

        --prefix <PREFIX>
            Extract only files whose paths begin with the given prefix.

        --password <PASSWORD>
            Read the archive password directly from the command line.

        --password-env <VAR>
            Read the archive password from the named environment variable.

        --password-file <PATH>
            Read the archive password from a file.

    NOTES
        If both --only and --prefix are specified, both conditions must match.

        Extraction loads only the needed chunks, but each referenced chunk is fully decompressed
        before the selected files are sliced out.

        If the archive is encrypted and no password source is specified, hpack prompts for the
        password before reading encrypted metadata.

VERIFY

    SYNOPSIS
        hpack verify <ARCHIVE>
            [--password <PASSWORD>]
            [--password-env <VAR>]
            [--password-file <PATH>]

    OPTIONS
        --password <PASSWORD>
            Read the archive password directly from the command line.

        --password-env <VAR>
            Read the archive password from the named environment variable.

        --password-file <PATH>
            Read the archive password from a file.

    CHECKS
        header/footer magic and structure
        archive version
        binary index CRC32
        file_count/chunk_count consistency
        dictionary placement
        chunk bounds
        duplicate file paths
        file spans inside each chunk
        per-file CRC32 against decompressed content

    NOTES
        If the archive is encrypted and no password source is specified, hpack prompts for the
        password before reading encrypted metadata.

TRAIN-DICT

    SYNOPSIS
        hpack train-dict <INPUT_DIR>...
            -o <DICT_FILE>
            [--max-samples <N>]
            [--dict-size <BYTES>]
            [--max-sample-bytes <BYTES>]
            [--include <GLOB>]...
            [--exclude <GLOB>]...
            [--extensions <EXT[,EXT...]>]
            [--mode all|text|html]

    OPTIONS
        <INPUT_DIR>...
            One or more input directories used as training corpora.

        -o, --output <DICT_FILE>
            Output dictionary file path.

        --max-samples <N>
            Maximum number of sampled files used for training.
            Default: 5000

        --dict-size <BYTES>
            Target dictionary size.
            Default: 131072

        --max-sample-bytes <BYTES>
            Maximum bytes taken from each sampled file.
            Default: 65536

        --include <GLOB>
            Include only files matching the given glob.
            May be specified multiple times.
            If omitted, there is no include restriction.

        --exclude <GLOB>
            Exclude files matching the given glob.
            May be specified multiple times.

        --extensions <EXT[,EXT...]>
            Restrict candidates by extension.
            Comma-separated. Leading dots are optional.
            Example:
                --extensions ini,json,txt,cfg

        --mode all|text|html
            Candidate selection preset.
            Default: text

            all
                accept all file types

            text
                accept common text-like extensions such as html, css, js, json, xml, txt,
                ini, cfg, toml, yaml, svg, sql, and similar

            html
                accept only .html and .htm

    FILTERING
        Candidate files are selected by combining:
            input directory scan
            --exclude
            optional --include
            optional --extensions
            --mode

        All filters are applied to archive-relative paths.

PATTERN MATCHING

    Glob patterns are matched against relative paths normalized with '/' separators.

    Examples:
        config/**
        *.json
        assets/*.png

EXAMPLES

    hpack pack ./dataset -o dataset.hpk
    hpack pack ./dataset -o dataset.hpk --chunk-size 33554432 --level 8
    hpack pack ./dataset -o dataset.hpk --dict app.dict --exclude "*.png"
    hpack pack ./dataset -o secure.hpk --password "correct horse battery staple"
    hpack pack ./dataset -o secure.hpk --password-env HPACK_PASSWORD
    hpack pack ./dataset -o secure.hpk --password-file ./archive.pass
    hpack pack ./dataset -o secure.hpk --password-prompt
    hpack list dataset.hpk
    hpack list secure.hpk --password "correct horse battery staple"
    hpack list secure.hpk --password-env HPACK_PASSWORD
    hpack extract dataset.hpk -d ./out
    hpack extract secure.hpk -d ./out --password "correct horse battery staple"
    hpack extract secure.hpk -d ./out --password-file ./archive.pass
    hpack extract dataset.hpk -d ./out --only "docs/index.html"
    hpack extract dataset.hpk -d ./out --prefix "docs/"
    hpack verify dataset.hpk
    hpack verify secure.hpk --password "correct horse battery staple"
    hpack verify secure.hpk --password-env HPACK_PASSWORD
    hpack train-dict ./snap1 ./snap2 ./snap3 -o app.dict --mode text --extensions ini,json,txt
    hpack train-dict ./app -o app.dict --include "config/**" --exclude "*.png" --dict-size 65536

EXIT STATUS

    0
        Success.

    non-zero
        Failure.

NOTES

    If pack is run without --dict, no dictionary is used.

    If pack is run without --password, --password-env, --password-file, or --password-prompt,
    no archive encryption is used.

    train-dict may fail if the corpus is too small for the requested --dict-size. In that case,
    reduce --dict-size or provide more training samples.

    The current public archive format is version 1. No legacy JSON-index format is supported.

    Passwords are accepted from the command line, an environment variable, a file, or an
    interactive prompt.

    Using --password is the least safe option because the password may be visible in shell
    history or process listings.
