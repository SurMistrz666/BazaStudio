//! Sentences for the result codes of the application. Internal codes are for
//! logs and tests; the interface shows a sentence in the user's language.
use baza_core::{ssh, updater};

/// Sentence for a result code. Text that is not a code is returned unchanged.
pub fn describe(code: &str, en: bool) -> String {
    let code = code.trim();
    if let Some(text) = ssh::describe_known(code, en) {
        return text.into();
    }
    if ["update_", "signed_", "signing_"]
        .iter()
        .any(|prefix| code.starts_with(prefix))
    {
        return updater::describe(code, en).into();
    }
    // The database reports its own error class next to our code.
    if let Some(state) = code
        .strip_prefix("database_error [")
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return if en {
            format!(
                "The database rejected the request (error code {state}). Check the statement or command and try again."
            )
        } else {
            format!(
                "Baza danych odrzuciła polecenie (kod błędu {state}). Sprawdź treść polecenia i spróbuj ponownie."
            )
        };
    }
    // A refused filter condition names what in it was refused.
    if let Some((code, detail)) = code
        .strip_suffix(']')
        .and_then(|rest| rest.split_once(" ["))
        && let Some(text) = condition_detail(code, detail, en)
    {
        return text;
    }
    // The keychain reports its own status number after these prefixes.
    if code.starts_with("vault_keychain_tool_error_") || code.starts_with("vault_keychain_error_") {
        return if en { KEYCHAIN.1 } else { KEYCHAIN.0 }.into();
    }
    if !looks_like_code(code) {
        return code.into();
    }
    // A driver explains the codes it defines itself.
    if let Some(text) = baza_core::drivers::describe(code, en) {
        return text.into();
    }
    let (pl, english) = known(code).unwrap_or_else(|| family(code));
    if en { english } else { pl }.into()
}

/// Sentence for a filter condition refused because of `detail`: a word or a
/// function of it, or the error class the database reported.
fn condition_detail(code: &str, detail: &str, en: bool) -> Option<String> {
    Some(match (code, en) {
        ("filter_condition_word", false) => format!(
            "Filtr: słowo {detail} jest niedozwolone. Warunek wybiera tylko wiersze tej tabeli; kolumnę o takiej nazwie ujmij w cudzysłów."
        ),
        ("filter_condition_word", true) => format!(
            "Filter: the word {detail} is not allowed. A condition only picks rows of this table; put a column with this name in quotes."
        ),
        ("filter_condition_function", false) => format!(
            "Filtr: funkcja {detail} jest niedozwolona. Warunek może używać funkcji, które tylko liczą wartość, np. LOWER, COALESCE, DATE."
        ),
        ("filter_condition_function", true) => format!(
            "Filter: the function {detail} is not allowed. A condition may use functions that only compute a value, such as LOWER, COALESCE, DATE."
        ),
        ("filter_condition_rejected", false) => format!(
            "Baza danych odrzuciła warunek filtra (kod błędu {detail}). Popraw warunek albo wyczyść pole filtra."
        ),
        ("filter_condition_rejected", true) => format!(
            "The database rejected the filter condition (error code {detail}). Correct the condition or clear the filter field."
        ),
        ("builder_value_word", false) => format!(
            "Warunek: słowo {detail} jest niedozwolone w wartości. Wartość to liczba, tekst albo kolumna; kolumnę o takiej nazwie ujmij w cudzysłów."
        ),
        ("builder_value_word", true) => format!(
            "Condition: the word {detail} is not allowed in a value. A value is a number, a text or a column; put a column with this name in quotes."
        ),
        ("builder_value_function", false) => format!(
            "Warunek: funkcja {detail} jest niedozwolona. Wartość może używać funkcji, które tylko liczą wartość, np. LOWER, COALESCE, DATE."
        ),
        ("builder_value_function", true) => format!(
            "Condition: the function {detail} is not allowed. A value may use functions that only compute a value, such as LOWER, COALESCE, DATE."
        ),
        _ => return None,
    })
}

/// The system keychain did not hand over the key of the vault.
const KEYCHAIN: (&str, &str) = (
    "Systemowy pęk kluczy nie zwrócił klucza sejfu.",
    "The system keychain did not return the vault key.",
);

/// A code is one lower-case word with underscores, without spaces.
fn looks_like_code(text: &str) -> bool {
    !text.is_empty()
        && text.contains('_')
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn known(code: &str) -> Option<(&'static str, &'static str)> {
    Some(match code {
        "query_cancelled" => ("Zapytanie zostało zatrzymane.", "The query was stopped."),
        "query_timeout" => (
            "Zapytanie trwało zbyt długo i zostało przerwane.",
            "The query took too long and was interrupted.",
        ),
        "database_error" => (
            "Baza danych odrzuciła polecenie. Sprawdź SQL i spróbuj ponownie.",
            "The database rejected the statement. Check the SQL and try again.",
        ),
        "database_login_rejected" => (
            "Baza danych odrzuciła nazwę użytkownika lub hasło. Sprawdź je na karcie Ogólne.",
            "The database rejected the user name or the password. Check them on the General tab.",
        ),
        "database_not_found" => (
            "Na serwerze nie ma bazy danych o tej nazwie. Sprawdź pole Baza danych na karcie Ogólne.",
            "The server has no database with this name. Check the Database field on the General tab.",
        ),
        "database_access_denied" => (
            "Ten użytkownik nie ma dostępu do wybranej bazy danych.",
            "This user has no access to the chosen database.",
        ),
        "database_certificate_rejected" => (
            "Certyfikat serwera bazy nie przeszedł weryfikacji: nieznany wystawca (na przykład certyfikat samopodpisany) albo inna nazwa hosta. Na karcie SSL/TLS wskaż certyfikat CA serwera albo wybierz tryb „Szyfrowanie wymagane”.",
            "The certificate of the database server could not be verified: an unknown issuer (for example a self-signed certificate) or another host name. On the SSL/TLS tab choose the CA certificate of the server, or the mode “Encryption required”.",
        ),
        "database_tls_unsupported" => (
            "Serwer bazy nie obsługuje połączeń szyfrowanych. Na karcie SSL/TLS wybierz tryb „Wyłączony”, jeśli łączysz się przez tunel SSH albo w zaufanej sieci.",
            "The database server does not support encrypted connections. On the SSL/TLS tab choose the mode “Disabled” if you connect through an SSH tunnel or inside a trusted network.",
        ),
        "database_network_error" => (
            "Utracono połączenie z bazą danych. Sprawdź sieć i połącz się ponownie.",
            "The connection to the database was lost. Check the network and connect again.",
        ),
        "database_tls_error" => (
            "Nie udało się zestawić szyfrowanego połączenia z bazą. Sprawdź ustawienia SSL/TLS i certyfikat CA.",
            "The encrypted connection to the database could not be set up. Check the SSL/TLS settings and the CA certificate.",
        ),
        "database_connection_timeout" => (
            "Baza danych nie odpowiedziała na czas. Sprawdź adres, port i dostępność serwera.",
            "The database did not answer in time. Check the address, the port and that the server is reachable.",
        ),
        "database_operation_failed" => (
            "Operacja na bazie danych nie powiodła się. Sprawdź dane logowania i uprawnienia.",
            "The database operation failed. Check the credentials and permissions.",
        ),
        "connection_test_timeout" => (
            "Serwer nie odpowiedział w ciągu 35 sekund. Sprawdź adres, port i zaporę sieciową.",
            "The server did not answer within 35 seconds. Check the address, the port and the firewall.",
        ),
        "connection_unavailable" => (
            "Połączenie nie jest aktywne. Połącz się z bazą i spróbuj ponownie.",
            "The connection is not active. Connect to the database and try again.",
        ),
        "connection_name_invalid" => (
            "Podaj nazwę połączenia (do 120 znaków).",
            "Enter a connection name (up to 120 characters).",
        ),
        "database_endpoint_invalid" => (
            "Podaj host i port bazy danych.",
            "Enter the database host and port.",
        ),
        "database_name_invalid" => ("Podaj nazwę bazy danych.", "Enter the database name."),
        // Kept under their first names: every engine that is a local file
        // reports them.
        "sqlite_path_required" => ("Wskaż plik bazy danych.", "Choose the database file."),
        "sqlite_ssh_unsupported" => (
            "Lokalny plik bazy nie korzysta z tunelu SSH.",
            "A local database file does not use an SSH tunnel.",
        ),
        "engine_ssh_unsupported" => (
            "Ten rodzaj bazy nie łączy się przez tunel SSH.",
            "This kind of database is not reached through an SSH tunnel.",
        ),
        "connection_product_invalid" | "connection_option_invalid" => (
            "Jedno z ustawień tego rodzaju bazy ma niedozwoloną wartość. Sprawdź pola na karcie Ogólne.",
            "One of the settings of this kind of database has a value that is not allowed. Check the fields on the General tab.",
        ),
        "operation_unsupported" => (
            "Ten rodzaj bazy nie obsługuje tej operacji.",
            "This kind of database does not support this operation.",
        ),
        "page_offset_unsupported" => (
            "Ten rodzaj bazy pokazuje tylko pierwszą stronę wyników.",
            "This kind of database shows only the first page of results.",
        ),
        "credential_too_large" => (
            "Hasło lub klucz jest zbyt długi.",
            "The password or key is too long.",
        ),
        "engine_unsupported" => (
            "Ten rodzaj bazy danych nie jest obsługiwany przez tę wersję programu.",
            "This kind of database is not supported by this version of the application.",
        ),
        "identifier_invalid" => (
            "Nazwa tabeli lub kolumny zawiera niedozwolone znaki.",
            "The table or column name contains characters that are not allowed.",
        ),
        "primary_key_required" => (
            "Ta tabela nie ma klucza głównego, więc pojedynczy wiersz nie może być zmieniony.",
            "This table has no primary key, so a single row cannot be changed.",
        ),
        "result_read_only" | "cell_read_only" => (
            "Ten wynik jest tylko do odczytu.",
            "This result is read-only.",
        ),
        "result_changed_reload_required" | "row_changed_reload_required" => (
            "Dane zmieniły się w międzyczasie. Odśwież wynik i spróbuj ponownie.",
            "The data changed in the meantime. Refresh the result and try again.",
        ),
        "column_not_found" => (
            "Nie znaleziono kolumny w tej tabeli.",
            "The column was not found in this table.",
        ),
        "column_null_forbidden" => (
            "Ta kolumna nie przyjmuje wartości NULL.",
            "This column does not accept NULL.",
        ),
        "column_name_duplicate" => (
            "Nazwy kolumn muszą być różne.",
            "Column names must be different.",
        ),
        "column_type_invalid" => (
            "Typ kolumny to nazwa z liter, cyfr i spacji, np. varchar albo int. Długość wpisz w osobnej kolumnie.",
            "A column type is a name of letters, digits and spaces, such as varchar or int. Put the length in its own column.",
        ),
        "column_length_invalid" => (
            "Długość i liczba miejsc po przecinku to liczby (albo MAX), a miejsca po przecinku wymagają długości.",
            "Length and decimals are numbers (or MAX), and decimals need a length.",
        ),
        "column_default_invalid" => (
            "Wartość domyślna to jedno wyrażenie, bez średnika i komentarzy, z zamkniętymi cudzysłowami i nawiasami.",
            "A default value is one expression, without a semicolon or comments, with its quotes and brackets closed.",
        ),
        "column_change_unsupported" => (
            "Ta baza nie zmieni tej kolumny w miejscu. Zmień ją poleceniem SQL w edytorze albo utwórz tabelę od nowa.",
            "This database cannot change this column in place. Change it with SQL in the editor or create the table again.",
        ),
        "primary_key_change_unsupported" => (
            "Klucz główny tej tabeli zmienisz tylko poleceniem SQL w edytorze: ma nazwę, którą trzeba podać.",
            "The primary key of this table can only be changed with SQL in the editor: it has a name that must be given.",
        ),
        "table_columns_invalid" => (
            "Tabela musi mieć nazwę i co najmniej jedną nazwaną kolumnę.",
            "The table needs a name and at least one named column.",
        ),
        "integer_invalid" => ("Wpisz liczbę całkowitą.", "Enter a whole number."),
        "decimal_invalid" | "float_invalid" | "number_invalid" => {
            ("Wpisz poprawną liczbę.", "Enter a valid number.")
        }
        "boolean_invalid" => ("Wpisz true albo false.", "Enter true or false."),
        "json_invalid" | "invalid_json" | "unsafe_json" => (
            "Wartość nie jest poprawnym dokumentem JSON.",
            "The value is not a valid JSON document.",
        ),
        "text_nul_unsupported" => (
            "Tekst zawiera znak, którego baza nie przyjmuje.",
            "The text contains a character the database does not accept.",
        ),
        "unsupported_cell_type" => (
            "Tego typu danych nie można tu edytować.",
            "This data type cannot be edited here.",
        ),
        "aggregate_invalid" | "aggregate_star_invalid" | "selection_invalid" => (
            "To podsumowanie nie pasuje do wybranej kolumny. Zmień funkcję albo kolumnę.",
            "This summary does not fit the chosen column. Change the function or the column.",
        ),
        "page_invalid" => (
            "Nie ma takiej strony wyników.",
            "There is no such page of results.",
        ),
        "sql_invalid" | "sql_unterminated_literal" => (
            "SQL jest niekompletny. Sprawdź cudzysłowy i średniki.",
            "The SQL is incomplete. Check the quotes and semicolons.",
        ),
        "column_values_invalid" => (
            "Pole ENUM albo SET potrzebuje co najmniej jednej wartości.",
            "An ENUM or SET field needs at least one value.",
        ),
        "index_invalid" => (
            "Indeks potrzebuje nazwy i co najmniej jednej kolumny; długość to liczba.",
            "An index needs a name and at least one column; a length is a number.",
        ),
        "foreign_key_invalid" => (
            "Klucz obcy potrzebuje nazwy, tabeli docelowej i par kolumn po obu stronach.",
            "A foreign key needs a name, a referenced table and pairs of columns on both sides.",
        ),
        "check_invalid" => (
            "Sprawdzenie potrzebuje nazwy i jednego warunku, bez średnika i komentarzy, z zamkniętymi cudzysłowami i nawiasami.",
            "A check needs a name and one condition, without a semicolon or comments, with its quotes and brackets closed.",
        ),
        "trigger_invalid" => (
            "Wyzwalacz potrzebuje nazwy i treści; DELIMITER nie jest potrzebny.",
            "A trigger needs a name and a body; DELIMITER is not needed.",
        ),
        "trigger_body_invalid" => (
            "Treść wyzwalacza to jedno polecenie albo jeden blok BEGIN … END, a po nim nic więcej. Sprawdź, czy każdy blok jest zamknięty (END IF, END CASE, END), czy cudzysłowy się zamykają i czy nie ma komentarzy /*! */.",
            "A trigger body is one statement or one BEGIN … END block, with nothing after it. Check that every block is closed (END IF, END CASE, END), that the quotes close and that there are no /*! */ comments.",
        ),
        "table_option_invalid" => (
            "Jedna z opcji tabeli ma niedozwoloną wartość.",
            "One of the table options has a value that is not allowed.",
        ),
        "partition_invalid" => (
            "Partycje są niepełne albo błędne: sprawdź wyrażenie, wartości każdej partycji (cudzysłowy i nawiasy muszą się zamykać) albo ich liczbę.",
            "The partitions are incomplete or wrong: check the expression, the values of each partition (quotes and brackets must close) or their count.",
        ),
        "partition_subpartitions_unsupported" => (
            "Ta tabela ma podpartycje, których projektant nie zmienia. Możesz usunąć podział („Bez partycji”) albo zmienić go poleceniem SQL w edytorze.",
            "This table has subpartitions, which the designer does not change. You can take the partitioning away (“No partitions”) or change it with SQL in the editor.",
        ),
        "sql_fragment_invalid" => (
            "Wpisany fragment SQL musi być jednym wyrażeniem: bez średnika i komentarzy, z zamkniętymi cudzysłowami i nawiasami.",
            "The SQL typed here must be one expression: without a semicolon or comments, with its quotes and brackets closed.",
        ),
        "index_name_duplicate" => (
            "Dwa indeksy mają tę samą nazwę.",
            "Two indexes have the same name.",
        ),
        "constraint_name_duplicate" => (
            "Dwa klucze obce albo sprawdzenia mają tę samą nazwę.",
            "Two foreign keys or checks have the same name.",
        ),
        "trigger_name_duplicate" => (
            "Dwa wyzwalacze mają tę samą nazwę.",
            "Two triggers have the same name.",
        ),
        "builder_no_table" => (
            "Dodaj tabelę na płótno kreatora.",
            "Put a table on the canvas of the builder.",
        ),
        "builder_field_invalid" => (
            "Jedna z kolumn należy do tabeli, której nie ma już w zapytaniu.",
            "One of the columns belongs to a table that is no longer in the query.",
        ),
        "builder_condition_value" => (
            "Warunek potrzebuje wartości: liczby, tekstu w apostrofach albo kolumny.",
            "A condition needs a value: a number, text in quotes or a column.",
        ),
        "builder_limit_invalid" => (
            "Liczba wierszy i pominiętych wierszy to liczby całkowite.",
            "The number of rows and of skipped rows are whole numbers.",
        ),
        "builder_offset_needs_limit" => (
            "Pomijanie wierszy w tej bazie wymaga też limitu wierszy.",
            "Skipping rows needs a row limit as well on this database.",
        ),
        "builder_value_too_long" => (
            "Warunek: wartość jest za długa.",
            "Condition: the value is too long.",
        ),
        "builder_value_unterminated" => (
            "Warunek: tekst albo nazwa w cudzysłowie nie jest zamknięta.",
            "Condition: a quoted text or name is not closed.",
        ),
        "builder_value_comment" => (
            "Warunek: komentarze (--, #, /* */) nie są dozwolone w wartości.",
            "Condition: comments (--, #, /* */) are not allowed in a value.",
        ),
        "builder_value_statements" => (
            "Warunek: wartość nie może zawierać średnika ani drugiego polecenia.",
            "Condition: a value cannot contain a semicolon or a second statement.",
        ),
        "builder_value_parentheses" => (
            "Warunek: nawiasy w wartości się nie domykają.",
            "Condition: the parentheses of a value do not match.",
        ),
        "builder_between_value" => (
            "Warunek BETWEEN potrzebuje dwóch wartości połączonych przez AND, np. 1 AND 10.",
            "A BETWEEN condition needs two values joined by AND, such as 1 AND 10.",
        ),
        "builder_relation_same_table" => (
            "Relacja łączy dwie kolumny tej samej tabeli. Aby połączyć tabelę z nią samą, dodaj ją na płótno drugi raz i połącz kolumny obu kopii.",
            "A relation joins two columns of the same table. To join a table to itself, put it on the canvas a second time and relate the columns of the two copies.",
        ),
        "engine_foreign_keys_unsupported" => (
            "Ten silnik nie obsługuje kluczy obcych. Usuń je na karcie „Klucze obce” albo zostań przy InnoDB.",
            "This engine does not keep foreign keys. Remove them on the “Foreign keys” page or stay with InnoDB.",
        ),
        "comment_invalid" => ("Komentarz jest za długi.", "The comment is too long."),
        "blob_layout_limit" => (
            "Zapisanych struktur jest za dużo albo jedna z nich jest za długa.",
            "Too many structures are kept, or one of them is too long.",
        ),
        "blob_layout_task_failed" => (
            "Nie udało się odczytać ani zapisać struktur kolumn.",
            "The structures of the columns could not be read or written.",
        ),
        "filter_condition_empty" => (
            "Filtr: wpisz warunek, np. id = 19 OR secret IS NULL.",
            "Filter: type a condition, such as id = 19 OR secret IS NULL.",
        ),
        "filter_condition_too_long" => (
            "Filtr: warunek jest za długi.",
            "Filter: the condition is too long.",
        ),
        "filter_condition_unterminated" => (
            "Filtr: tekst albo nazwa w cudzysłowie nie jest zamknięta.",
            "Filter: a quoted text or name is not closed.",
        ),
        "filter_condition_comment" => (
            "Filtr: komentarze (--, #, /* */) nie są dozwolone w warunku.",
            "Filter: comments (--, #, /* */) are not allowed in a condition.",
        ),
        "filter_condition_statements" => (
            "Filtr: warunek nie może zawierać średnika ani drugiego polecenia.",
            "Filter: a condition cannot contain a semicolon or a second statement.",
        ),
        "filter_condition_parentheses" => (
            "Filtr: nawiasy w warunku się nie domykają.",
            "Filter: the parentheses of the condition do not match.",
        ),
        "filter_condition_rejected" => (
            "Baza danych odrzuciła warunek filtra. Popraw warunek albo wyczyść pole filtra.",
            "The database rejected the filter condition. Correct the condition or clear the filter field.",
        ),
        "sql_too_many_statements" => (
            "Plik zawiera zbyt wiele poleceń SQL.",
            "The file contains too many SQL statements.",
        ),
        "sql_file_too_large" | "sql_file_invalid" => (
            "Ten plik SQL jest za duży, aby otworzyć go w edytorze.",
            "This SQL file is too large to open in the editor.",
        ),
        "sql_utf8_required" => (
            "Plik SQL musi być zapisany w kodowaniu UTF-8.",
            "The SQL file must be saved as UTF-8.",
        ),
        "sql_save_failed" => (
            "Nie udało się zapisać pliku SQL. Sprawdź miejsce i uprawnienia do folderu.",
            "The SQL file could not be saved. Check the location and folder permissions.",
        ),
        "sql_export_requires_table" => (
            "Eksport do SQL wymaga wyniku z jednej tabeli. Wybierz inny format albo otwórz dane tabeli.",
            "SQL export needs a result from one table. Choose another format or open the table data.",
        ),
        "export_too_large" => (
            "Wynik jest za duży, aby zapisać go w jednym pliku.",
            "The result is too large to save in one file.",
        ),
        "export_write_failed" | "export_task_failed" | "profile_export_failed" => (
            "Nie udało się zapisać pliku. Sprawdź miejsce i uprawnienia do folderu.",
            "The file could not be saved. Check the location and folder permissions.",
        ),
        "format_unsupported" | "import_format_unsupported" => (
            "Ten format pliku nie jest obsługiwany. Wybierz CSV, TSV albo JSON.",
            "This file format is not supported. Choose CSV, TSV or JSON.",
        ),
        "import_file_too_large" | "import_rows_too_many" | "import_row_too_large" => (
            "Plik jest za duży na jeden import (limit 5 MB). Podziel go na mniejsze części.",
            "The file is too large for one import (5 MB limit). Split it into smaller parts.",
        ),
        "import_column_unknown" | "import_columns_invalid" | "header_invalid" => (
            "Nagłówki pliku nie pasują do kolumn tabeli. Dopasuj nazwy kolumn.",
            "The file headers do not match the table columns. Map the column names.",
        ),
        "import_shape_invalid" | "csv_record_metadata_invalid" | "csv_unterminated_quote" => (
            "Plik ma niespójne wiersze. Sprawdź separatory i cudzysłowy.",
            "The file has inconsistent rows. Check the separators and quotes.",
        ),
        "file_read_failed" => (
            "Nie udało się odczytać pliku.",
            "The file could not be read.",
        ),
        "file_too_large" | "file_limit" | "json_limit" => {
            ("Ten plik jest za duży.", "This file is too large.")
        }
        "saved_query_name_exists" => (
            "Zapytanie o tej nazwie już istnieje. Wybierz inną nazwę.",
            "A query with this name already exists. Choose another name.",
        ),
        "saved_query_limit" | "saved_query_storage_limit" => (
            "Biblioteka zapytań jest pełna. Usuń nieużywane zapytania.",
            "The query library is full. Delete queries you no longer use.",
        ),
        "group_name_exists" => (
            "Grupa o tej nazwie już istnieje na tym poziomie.",
            "A group with this name already exists at this level.",
        ),
        "group_name_invalid" => (
            "Podaj nazwę grupy (do 64 znaków).",
            "Enter a group name (up to 64 characters).",
        ),
        "group_count_limit" | "group_depth_limit" | "group_storage_limit" => (
            "Osiągnięto limit grup połączeń.",
            "The limit of connection groups was reached.",
        ),
        "group_cycle" => (
            "Grupa nie może znaleźć się wewnątrz samej siebie.",
            "A group cannot be placed inside itself.",
        ),
        "group_revision_changed" => (
            "Grupy zmieniły się w innym oknie. Lista została odświeżona; spróbuj ponownie.",
            "The groups changed in another window. The list was refreshed; try again.",
        ),
        "vault_unavailable" => (
            "Lokalny sejf nie jest jeszcze gotowy. Spróbuj ponownie za chwilę.",
            "The local vault is not ready yet. Try again in a moment.",
        ),
        "vault_conflict_reload" | "vault_snapshot_changed" => (
            "Profile zmieniły się w innym oknie programu. Dane zostały odświeżone; spróbuj ponownie.",
            "The profiles changed in another window of the application. The data was refreshed; try again.",
        ),
        "vault_busy" | "vault_lock_failed" => (
            "Lokalny sejf jest zajęty przez inne okno programu. Spróbuj ponownie za chwilę.",
            "The local vault is in use by another window of the application. Try again in a moment.",
        ),
        "vault_keychain_access_denied" => (
            "macOS nie pozwolił odczytać klucza sejfu zapisanego przez starszą wersję. Uruchom aplikację ponownie i wybierz „Zawsze pozwalaj”. Dane nie zostały zmienione.",
            "macOS did not allow access to the vault key saved by an earlier version. Reopen the app and choose Always Allow. Your data has not been changed.",
        ),
        "vault_keychain_approval_timeout" | "vault_keychain_approval_pending" => (
            "Czekamy na odpowiedź w oknie pęku kluczy macOS. Wybierz „Zawsze pozwalaj”, a potem uruchom aplikację ponownie.",
            "Waiting for your answer to the macOS keychain prompt. Choose Always Allow, then reopen the app.",
        ),
        "vault_keychain_approval_required"
        | "vault_keychain_tool_timeout"
        | "vault_keychain_tool_unavailable"
        | "vault_keychain_tool_invalid" => KEYCHAIN,
        "vault_keychain_locked" => (
            "Pęk kluczy systemu jest zablokowany. Odblokuj go i spróbuj ponownie.",
            "The system keychain is locked. Unlock it and try again.",
        ),
        "local_unlock_invalid" | "local_totp_invalid" => (
            "Hasło lub kod są nieprawidłowe.",
            "The password or code is incorrect.",
        ),
        "local_retry_later" => (
            "Zbyt wiele nieudanych prób. Odczekaj chwilę i spróbuj ponownie.",
            "Too many failed attempts. Wait a moment and try again.",
        ),
        "local_password_length" => (
            "Hasło lokalne musi mieć co najmniej 12 znaków.",
            "The local password needs at least 12 characters.",
        ),
        "local_enrollment_expired" | "local_enrollment_missing" | "local_enrollment_invalid" => (
            "Konfiguracja blokady wygasła. Zacznij ją od nowa.",
            "The lock setup expired. Start it again.",
        ),
        "local_reauthentication_required" => (
            "Potwierdź zmianę hasłem lokalnym.",
            "Confirm the change with your local password.",
        ),
        "local_clock_invalid" => (
            "Zegar systemowy ma nieprawidłową datę. Ustaw go i spróbuj ponownie.",
            "The system clock has an invalid date. Set it and try again.",
        ),
        "passphrase_required" => (
            "Ten plik jest zaszyfrowany. Podaj jego hasło.",
            "This file is encrypted. Enter its passphrase.",
        ),
        "passphrase_invalid" | "bundle_authentication_failed" => (
            "Hasło pliku jest nieprawidłowe albo plik został uszkodzony.",
            "The file passphrase is incorrect or the file is damaged.",
        ),
        "bundle_header_invalid" | "profiles_invalid" | "profile_kind_unsupported" => (
            "To nie jest plik profili Baza Studio.",
            "This is not a Baza Studio profile file.",
        ),
        "plaintext_credentials_ignored" => (
            "Plik JSON nie zawiera haseł ani kluczy. Uzupełnij je po imporcie.",
            "A JSON file carries no passwords or keys. Enter them after the import.",
        ),
        "ssh_required" => (
            "W pliku brakuje ustawień SSH tego wpisu.",
            "The file lacks the SSH settings of this entry.",
        ),
        "profile_correction_required" => (
            "Część profili wymaga uzupełnienia przed importem.",
            "Some profiles need to be completed before the import.",
        ),
        "profile_duplicate" => ("Taki profil już istnieje.", "This profile already exists."),
        "profile_limit" => (
            "Osiągnięto limit zapisanych profili.",
            "The limit of saved profiles was reached.",
        ),
        "port_invalid" => ("Podaj port od 1 do 65535.", "Enter a port from 1 to 65535."),
        "tls_mode_invalid" => (
            "Wybierz tryb SSL/TLS z listy.",
            "Choose an SSL/TLS mode from the list.",
        ),
        "preferences_invalid" => (
            "Preferencje mają nieprawidłowe wartości. Przywróć domyślne i zapisz ponownie.",
            "The preferences contain invalid values. Restore the defaults and save again.",
        ),
        "native_os_vault_unsupported" => (
            "Ten system nie udostępnia bezpiecznego magazynu kluczy wymaganego przez sejf.",
            "This system does not provide the secure key store the vault needs.",
        ),
        _ => return None,
    })
}

/// Sentence for a whole family of codes without an entry of their own.
fn family(code: &str) -> (&'static str, &'static str) {
    let prefix = code.split('_').next().unwrap_or_default();
    match prefix {
        "vault" | "kdf" | "cipher" => (
            "Nie udało się odczytać lub zapisać lokalnego sejfu. Sprawdź uprawnienia do folderu danych programu i spróbuj ponownie.",
            "The local vault could not be read or written. Check the permissions of the application data folder and try again.",
        ),
        "local" => (
            "Nie udało się zmienić blokady aplikacji. Spróbuj ponownie.",
            "The application lock could not be changed. Try again.",
        ),
        "group" => (
            "Nie udało się zapisać grup połączeń. Odśwież grupy i spróbuj ponownie.",
            "The connection groups could not be saved. Refresh the groups and try again.",
        ),
        "saved" => (
            "Nie udało się zapisać biblioteki zapytań. Spróbuj ponownie.",
            "The query library could not be saved. Try again.",
        ),
        "import" | "csv" => (
            "Nie udało się zaimportować pliku. Sprawdź jego format i spróbuj ponownie.",
            "The file could not be imported. Check its format and try again.",
        ),
        "export" => (
            "Nie udało się wyeksportować danych. Spróbuj ponownie.",
            "The data could not be exported. Try again.",
        ),
        "profile" | "profiles" | "bundle" => (
            "Nie udało się przetworzyć pliku profili. Sprawdź plik i spróbuj ponownie.",
            "The profile file could not be processed. Check the file and try again.",
        ),
        "connection" | "database" => (
            "Nie udało się połączyć z bazą danych. Sprawdź ustawienia połączenia i spróbuj ponownie.",
            "The database connection failed. Check the connection settings and try again.",
        ),
        "history" => (
            "Nie udało się zapisać historii zapytań. Zapytanie nie zostało uruchomione.",
            "The query history could not be saved. The query was not run.",
        ),
        "sql" => (
            "Nie udało się przetworzyć SQL. Sprawdź polecenie i spróbuj ponownie.",
            "The SQL could not be processed. Check the statement and try again.",
        ),
        _ => (
            "Operacja nie powiodła się. Spróbuj ponownie.",
            "The operation failed. Try again.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::describe;

    #[test]
    fn codes_never_reach_the_interface() {
        for code in [
            "vault_unavailable",
            "query_cancelled",
            "database_error [42601]",
            "database_network_error",
            "database_login_rejected",
            "database_certificate_rejected",
            "database_tls_unsupported",
            "engine_ssh_unsupported",
            "connection_option_invalid",
            "operation_unsupported",
            "page_offset_unsupported",
            "database_not_found",
            "database_access_denied",
            "tunnel_target_refused",
            "ssh_host_key_changed",
            "update_task_failed",
            "group_revision_changed",
            "some_future_code",
            "vault_acl_read_failed",
            "vault_keychain_access_denied",
            "vault_keychain_approval_timeout",
            "vault_keychain_approval_pending",
            "vault_keychain_approval_required",
            "vault_keychain_tool_timeout",
            "vault_keychain_tool_unavailable",
            "vault_keychain_tool_invalid",
            "vault_keychain_tool_error_44",
            "vault_keychain_error_-25293",
            "vault_task_failed",
            "filter_condition_empty",
            "filter_condition_too_long",
            "filter_condition_unterminated",
            "filter_condition_comment",
            "filter_condition_statements",
            "filter_condition_parentheses",
            "filter_condition_rejected",
            "filter_condition_rejected [42601]",
            "filter_condition_word [SELECT]",
            "filter_condition_function [SLEEP]",
            "builder_value_too_long",
            "builder_value_unterminated",
            "builder_value_comment",
            "builder_value_statements",
            "builder_value_parentheses",
            "builder_value_word [SELECT]",
            "builder_value_function [SLEEP]",
            "builder_between_value",
            "builder_relation_same_table",
            "trigger_body_invalid",
            "partition_subpartitions_unsupported",
            "sql_fragment_invalid",
        ] {
            for en in [false, true] {
                let text = describe(code, en);
                assert!(!text.contains('_'), "{code} -> {text}");
                assert!(text.ends_with('.'), "{code} -> {text}");
            }
        }
        assert!(describe("database_error [42601]", true).contains("42601"));
        assert_eq!(
            describe("vault_keychain_error_-25293", true),
            "The system keychain did not return the vault key."
        );
        assert_eq!(
            describe("vault_keychain_approval_pending", false),
            describe("vault_keychain_approval_timeout", false)
        );
        assert!(describe("vault_keychain_access_denied", false).contains("Zawsze pozwalaj"));
    }

    #[test]
    fn sentences_pass_through_unchanged() {
        assert_eq!(describe("Zapisano plik SQL.", false), "Zapisano plik SQL.");
        assert_eq!(describe("Connected.", true), "Connected.");
    }
}
