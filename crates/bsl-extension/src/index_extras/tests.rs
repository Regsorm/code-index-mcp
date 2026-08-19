use super::*;
use code_index_core::storage::Storage;
use std::io::Write;
use tempfile::TempDir;

fn fresh_storage(tmp: &TempDir) -> Storage {
    let db_path = tmp.path().join("index.db");
    let storage = Storage::open_file(&db_path).unwrap();
    storage.apply_schema_extensions(crate::schema::SCHEMA_EXTENSIONS).unwrap();
    storage
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(path)
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
}

#[test]
fn incremental_config_change_adds_new_object() {
    // Фаза 3: добавление объекта в состав. Опись (ConfigDumpInfo.xml) в
    // батче — триггер сверки реестра; сам объект индексируется своим
    // корневым XML через пофайловую ветку (upsert_metadata_object).
    // Результат эквивалентен полному run_index_extras.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Контрагенты" id="k1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let cnt = |st: &Storage| -> i64 {
        st.conn()
            .query_row(
                "SELECT COUNT(*) FROM metadata_objects WHERE repo = ?",
                params![REPO_DEFAULT],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(cnt(&storage), 1, "исходно один объект");

    // Добавили Склады: Configuration.xml + опись + корневой XML объекта + .bsl.
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog><Catalog>Склады</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Контрагенты" id="k1" configVersion="v1"/><Metadata name="Catalog.Склады" id="s1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
    let sklady_xml = repo.join("Catalogs").join("Склады.xml");
    write(
        &sklady_xml,
        r#"<MetaDataObject><Catalog><Properties><Name>Склады</Name></Properties></Catalog></MetaDataObject>"#,
    );
    let bsl = repo
        .join("Catalogs")
        .join("Склады")
        .join("Ext")
        .join("ManagerModule.bsl");
    write(&bsl, "Процедура П() Экспорт\nКонецПроцедуры");

    let dump = repo.join("ConfigDumpInfo.xml");
    run_incremental_extras(
        &repo,
        &mut storage,
        &[repo.join("Configuration.xml"), dump, sklady_xml, bsl],
        &[],
    )
    .unwrap();

    let tmp2 = TempDir::new().unwrap();
    let mut full = fresh_storage(&tmp2);
    run_index_extras(&repo, &mut full).unwrap();

    assert_eq!(cnt(&storage), 2, "новый объект Склады заведён");
    assert_eq!(cnt(&storage), cnt(&full), "incremental metadata_objects == full");
}

// Набор full_name объектов репо (сортированный) — надёжнее COUNT: ловит и
// переименование (число строк не меняется, а состав имён — да).
#[cfg(test)]
fn object_names(st: &Storage) -> Vec<String> {
    let conn = st.conn();
    let mut s = conn
        .prepare("SELECT full_name FROM metadata_objects WHERE repo = ? ORDER BY full_name")
        .unwrap();
    let rows = s.query_map(params![REPO_DEFAULT], |r| r.get(0)).unwrap();
    rows.map(|x| x.unwrap()).collect()
}

#[test]
fn incremental_config_change_removes_object() {
    // Фаза 3: удаление объекта из состава. Опись без объекта в батче →
    // reconcile_area каскадно убирает объект (дом уронил). Эквивалентно
    // полному пересбору.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog><Catalog>Склады</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Контрагенты" id="k1" configVersion="v1"/><Metadata name="Catalog.Склады" id="s1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
    write(
        &repo.join("Catalogs").join("Контрагенты.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Контрагенты</Name></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("Catalogs").join("Склады.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Склады</Name></Properties></Catalog></MetaDataObject>"#,
    );
    let sklady_bsl = repo
        .join("Catalogs")
        .join("Склады")
        .join("Ext")
        .join("ManagerModule.bsl");
    write(&sklady_bsl, "Процедура П() Экспорт\nКонецПроцедуры");
    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    assert_eq!(object_names(&storage).len(), 2, "исходно два объекта");

    // Удалили Склады: опись без него + удаление его файлов с диска.
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Контрагенты" id="k1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
    let sklady_xml = repo.join("Catalogs").join("Склады.xml");
    std::fs::remove_file(&sklady_xml).ok();
    std::fs::remove_file(&sklady_bsl).ok();
    let dump = repo.join("ConfigDumpInfo.xml");
    run_incremental_extras(
        &repo,
        &mut storage,
        &[repo.join("Configuration.xml"), dump],
        &[sklady_xml, sklady_bsl],
    )
    .unwrap();

    let tmp2 = TempDir::new().unwrap();
    let mut full = fresh_storage(&tmp2);
    run_index_extras(&repo, &mut full).unwrap();

    assert_eq!(
        object_names(&storage),
        object_names(&full),
        "incremental: удалённый объект убран из metadata_objects (== full)"
    );
}

#[test]
fn incremental_config_change_reflects_rename() {
    // Фаза 3: переименование = удаление старого + добавление нового. Опись
    // со сменой имени в батче → reconcile_area каскадно сносит старый;
    // новый объект индексируется своим корневым XML. Сверяем НАБОР имён.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects><Catalog>Старый</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Старый" id="o1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
    write(
        &repo.join("Catalogs").join("Старый.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Старый</Name></Properties></Catalog></MetaDataObject>"#,
    );
    let old_bsl = repo
        .join("Catalogs")
        .join("Старый")
        .join("Ext")
        .join("ManagerModule.bsl");
    write(&old_bsl, "Процедура П() Экспорт\nКонецПроцедуры");
    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    assert_eq!(object_names(&storage), vec!["Catalog.Старый".to_string()]);

    // Переименовали Старый → Новый.
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects><Catalog>Новый</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Новый" id="n1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
    let old_xml = repo.join("Catalogs").join("Старый.xml");
    std::fs::remove_file(&old_xml).ok();
    std::fs::remove_file(&old_bsl).ok();
    let new_xml = repo.join("Catalogs").join("Новый.xml");
    write(
        &new_xml,
        r#"<MetaDataObject><Catalog><Properties><Name>Новый</Name></Properties></Catalog></MetaDataObject>"#,
    );
    let new_bsl = repo
        .join("Catalogs")
        .join("Новый")
        .join("Ext")
        .join("ManagerModule.bsl");
    write(&new_bsl, "Процедура П() Экспорт\nКонецПроцедуры");
    let dump = repo.join("ConfigDumpInfo.xml");
    run_incremental_extras(
        &repo,
        &mut storage,
        &[repo.join("Configuration.xml"), dump, new_xml, new_bsl],
        &[old_xml, old_bsl],
    )
    .unwrap();

    let tmp2 = TempDir::new().unwrap();
    let mut full = fresh_storage(&tmp2);
    run_index_extras(&repo, &mut full).unwrap();

    assert_eq!(
        object_names(&storage),
        object_names(&full),
        "incremental отразил переименование объекта (== full)"
    );
}

#[test]
fn fills_metadata_objects_from_configuration_xml() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject>
  <Configuration>
<ChildObjects>
  <Catalog>Контрагенты</Catalog>
  <Document>РеализацияТоваровУслуг</Document>
</ChildObjects>
  </Configuration>
</MetaDataObject>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM metadata_objects WHERE repo = ?",
            params![REPO_DEFAULT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn nested_subsystem_synonym_filled() {
    // Синонимы собирались обходом ровно на один уровень внутри папки типа, а
    // вложенная подсистема лежит на два уровня глубже — её шапка не читалась
    // вовсе, и синоним оставался пустым (E-6).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Subsystem>Продажи</Subsystem>
</ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("Subsystems").join("Продажи.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Subsystem><Properties>
  <Name>Продажи</Name>
  <Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Продажи и оплаты</v8:content></v8:item></Synonym>
</Properties><ChildObjects/></Subsystem></MetaDataObject>"#,
    );
    write(
        &repo.join("Subsystems").join("Продажи").join("Subsystems").join("Розница.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Subsystem><Properties>
  <Name>Розница</Name>
  <Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Розничные продажи</v8:content></v8:item></Synonym>
</Properties><ChildObjects/></Subsystem></MetaDataObject>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();

    let syn = |full: &str| -> Option<String> {
        storage
            .conn()
            .query_row(
                "SELECT synonym FROM metadata_objects WHERE repo=?1 AND full_name=?2",
                params![REPO_DEFAULT, full],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
    };
    assert_eq!(syn("Subsystem.Продажи").as_deref(), Some("Продажи и оплаты"));
    assert_eq!(
        syn("Subsystem.Розница").as_deref(),
        Some("Розничные продажи"),
        "у вложенной подсистемы синоним обязан заполняться"
    );
}

#[test]
fn incremental_bsl_delete_removes_metadata_module_row() {
    // Удаление .bsl обязано убирать строку модуля — как это делает полный
    // пересбор. Раньше пофайловая ветка умела только заводить и обновлять,
    // и строка оставалась указывать на несуществующий файл (E-5).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?><MetaDataObject><Configuration><ChildObjects><Catalog>Склады</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("Catalogs").join("Склады.xml"),
        r#"<?xml version="1.0"?><MetaDataObject><Catalog uuid="11111111-1111-1111-1111-111111111111"><Properties><Name>Склады</Name></Properties></Catalog></MetaDataObject>"#,
    );
    let bsl = repo.join("Catalogs").join("Склады").join("Ext").join("ManagerModule.bsl");
    write(&bsl, "Процедура П() Экспорт
КонецПроцедуры");

    let count = |st: &Storage| -> i64 {
        st.conn()
            .query_row(
                "SELECT COUNT(*) FROM metadata_modules WHERE repo=?1 AND module_type='ManagerModule'",
                params![REPO_DEFAULT],
                |r| r.get(0),
            )
            .unwrap()
    };

    let mut inc = fresh_storage(&tmp);
    run_index_extras(&repo, &mut inc).unwrap();
    assert_eq!(count(&inc), 1, "после полной индексации строка модуля есть");

    // Модуль удалён с диска, событие пришло как deleted.
    std::fs::remove_file(&bsl).unwrap();
    run_incremental_extras(&repo, &mut inc, &[], &[bsl.clone()]).unwrap();
    assert_eq!(count(&inc), 0, "строка удалённого модуля обязана исчезнуть");

    // Эталон: полный пересбор на том же дереве строки тоже не заводит.
    let tmp_full = TempDir::new().unwrap();
    let mut full = fresh_storage(&tmp_full);
    run_index_extras(&repo, &mut full).unwrap();
    assert_eq!(count(&full), 0, "инкремент совпадает с полным пересбором");
}

#[test]
fn object_command_module_gets_row_with_command_uuid() {
    // Модуль команды объекта раньше не попадал в перечень вовсе: владельцем
    // объявлялась сама команда и искался несуществующий файл
    // `Commands/<Команда>.xml` (E-7).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?><MetaDataObject><Configuration><ChildObjects><Catalog>Склады</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("Catalogs").join("Склады.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Catalog uuid="11111111-1111-1111-1111-111111111111">
  <Properties><Name>Склады</Name></Properties>
  <ChildObjects>
<Command uuid="cccccccc-2222-3333-4444-555555555555">
  <Properties><Name>ПечатьЭтикеток</Name></Properties>
</Command>
<Command uuid="dddddddd-6666-7777-8888-999999999999">
  <Properties><Name>Инвентаризация</Name></Properties>
</Command>
  </ChildObjects>
</Catalog></MetaDataObject>"#,
    );
    write(
        &repo
            .join("Catalogs")
            .join("Склады")
            .join("Commands")
            .join("Инвентаризация")
            .join("Ext")
            .join("CommandModule.bsl"),
        "Процедура ОбработкаКоманды() Экспорт
КонецПроцедуры",
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();

    let row: (String, String, String) = storage
        .conn()
        .query_row(
            "SELECT object_name, full_name, object_id FROM metadata_modules                  WHERE repo=?1 AND module_type='CommandModule'",
            params![REPO_DEFAULT],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("модуль команды объекта обязан попадать в перечень");
    assert_eq!(row.0, "Catalogs.Склады.Command.Инвентаризация");
    assert_eq!(row.1, "Catalogs.Склады.Command.Инвентаризация.CommandModule");
    assert_eq!(
        row.2, "dddddddd-6666-7777-8888-999999999999",
        "идентификатор берётся у самой команды, а не у объекта"
    );
}

#[test]
fn all_object_folders_cover_known_meta_types() {
    // upsert перечня должен покрывать те же типы, что index_metadata_objects
    // вносит из Configuration.xml (все KNOWN_META_TYPES). Пропуск типа =
    // тихая дыра после снятия config_changed-триггера (Фаза 2).
    use crate::xml::configuration::KNOWN_META_TYPES;
    for mt in KNOWN_META_TYPES {
        assert!(
            ALL_OBJECT_FOLDERS.iter().any(|(_folder, t)| t == mt),
            "meta_type {} не покрыт ALL_OBJECT_FOLDERS — upsert перечня его пропустит",
            mt
        );
    }
}

#[test]
fn upsert_metadata_object_owner_and_synonym_base_first() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    // База: объект Контрагенты (без ObjectBelonging → владелец база).
    write(
        &repo.join("base").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("Catalogs").join("Контрагенты.xml"),
        r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core"><Catalog><Properties><Name>Контрагенты</Name><Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Контрагенты база</v8:content></v8:item></Synonym></Properties></Catalog></MetaDataObject>"#,
    );
    // Расширение EF_A: Контрагенты заимствован (Adopted) + собственный Native.
    write(
        &repo.join("extensions").join("EF_A").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog><Catalog>МойОбъект</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("extensions").join("EF_A").join("Catalogs").join("Контрагенты.xml"),
        r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core"><Catalog><Properties><Name>Контрагенты</Name><Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Контрагенты расш</v8:content></v8:item></Synonym><ObjectBelonging>Adopted</ObjectBelonging></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("extensions").join("EF_A").join("Catalogs").join("МойОбъект.xml"),
        r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core"><Catalog><Properties><Name>МойОбъект</Name><Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Мой</v8:content></v8:item></Synonym><ObjectBelonging>Native</ObjectBelonging></Properties></Catalog></MetaDataObject>"#,
    );

    let storage = fresh_storage(&tmp);
    let conn = storage.conn();

    // Заимствованный объект (есть копия в base): владелец '', синоним base-first
    // (base перебивает расширенческий "Контрагенты расш").
    upsert_metadata_object(
        &repo,
        conn,
        &sub_config_roots(&repo),
        &repo.join("base").join("Catalogs").join("Контрагенты.xml"),
    )
    .unwrap();
    let (syn, sub): (Option<String>, String) = conn
        .query_row(
            "SELECT synonym, sub_config FROM metadata_objects WHERE full_name = ?",
            params!["Catalog.Контрагенты"],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(syn.as_deref(), Some("Контрагенты база"), "синоним base-first");
    assert_eq!(sub, "", "Adopted/base → владелец база ''");

    // Собственный объект расширения (Native, только в ext): владелец = путь расширения.
    upsert_metadata_object(
        &repo,
        conn,
        &sub_config_roots(&repo),
        &repo.join("extensions").join("EF_A").join("Catalogs").join("МойОбъект.xml"),
    )
    .unwrap();
    let (syn2, sub2): (Option<String>, String) = conn
        .query_row(
            "SELECT synonym, sub_config FROM metadata_objects WHERE full_name = ?",
            params!["Catalog.МойОбъект"],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(syn2.as_deref(), Some("Мой"));
    assert_eq!(sub2, "extensions/EF_A", "Native → владелец = путь расширения");
}

#[test]
fn fills_config_manifest_from_all_areas() {
    // Реестр config_manifest наполняется из ConfigDumpInfo.xml каждой области
    // (base + расширения). Проверяем: формат area совпадает с sub_config
    // ('' / 'extensions/EF_A'), заимствованный объект попадает в ОБЕ области,
    // под-элемент хранится с пустым config_version.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    write(&repo.join("base").join("Configuration.xml"), "<MetaDataObject/>");
    write(
        &repo.join("base").join("ConfigDumpInfo.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ConfigDumpInfo><ConfigVersions>
  <Metadata name="Catalog.Контрагенты" id="c-uuid" configVersion="cbase">
<Metadata name="Catalog.Контрагенты.Attribute.ИНН" id="a-uuid"/>
  </Metadata>
  <Metadata name="Document.ЗаказКлиента" id="d-uuid" configVersion="zbase"/>
</ConfigVersions></ConfigDumpInfo>"#,
    );
    write(
        &repo.join("extensions").join("EF_A").join("Configuration.xml"),
        "<MetaDataObject/>",
    );
    write(
        &repo.join("extensions").join("EF_A").join("ConfigDumpInfo.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ConfigDumpInfo><ConfigVersions>
  <Metadata name="Catalog.Контрагенты" id="c-uuid" configVersion="cext"/>
  <Metadata name="Catalog.МойОбъект" id="m-uuid" configVersion="mynative"/>
</ConfigVersions></ConfigDumpInfo>"#,
    );

    let storage = fresh_storage(&tmp);
    let conn = storage.conn();
    index_config_manifest(&repo, conn).unwrap();

    // Всего 5 строк: base(3) + ext(2).
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM config_manifest", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 5);

    // area базы = '' с версией объекта.
    let cv_base: String = conn
        .query_row(
            "SELECT config_version FROM config_manifest WHERE area = '' AND full_name = 'Catalog.Контрагенты'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cv_base, "cbase");

    // area расширения = 'extensions/EF_A' — тот же формат, что metadata_objects.sub_config.
    let cv_ext: String = conn
        .query_row(
            "SELECT config_version FROM config_manifest WHERE area = 'extensions/EF_A' AND full_name = 'Catalog.Контрагенты'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cv_ext, "cext");

    // Заимствованный объект числится в ДВУХ областях (ключевая предпосылка Фазы 2).
    let areas: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT area) FROM config_manifest WHERE full_name = 'Catalog.Контрагенты'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(areas, 2, "заимствованный объект — в base и в расширении");

    // Под-элемент: config_version пустой.
    let cv_sub: String = conn
        .query_row(
            "SELECT config_version FROM config_manifest WHERE full_name = 'Catalog.Контрагенты.Attribute.ИНН'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cv_sub, "", "под-элемент хранится без configVersion");

    // Native-объект расширения — только в его области.
    let native_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM config_manifest WHERE full_name = 'Catalog.МойОбъект'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(native_rows, 1);
}

#[test]
fn reconcile_home_delete_cascades_object() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    write(
        &repo.join("base").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Удаляемый</Catalog><Catalog>Живой</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("Catalogs").join("Удаляемый.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Удаляемый</Name></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("Catalogs").join("Живой.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Живой</Name></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions>
  <Metadata name="Catalog.Удаляемый" id="u1" configVersion="v1">
<Metadata name="Catalog.Удаляемый.Attribute.Реквизит" id="a1"/>
  </Metadata>
  <Metadata name="Catalog.Живой" id="z1" configVersion="v2"/>
</ConfigVersions></ConfigDumpInfo>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();
    let cnt1 = |c: &rusqlite::Connection, sql: &str, p: &str| -> i64 {
        c.query_row(sql, params![p], |r| r.get::<_, i64>(0)).unwrap()
    };
    assert_eq!(cnt1(conn, "SELECT COUNT(*) FROM metadata_objects WHERE full_name = ?", "Catalog.Удаляемый"), 1);
    assert_eq!(cnt1(conn, "SELECT COUNT(*) FROM config_manifest WHERE full_name = ?", "Catalog.Удаляемый"), 1);

    // Удаляемый пропал из свежей описи (Живой остался) — уронила домашняя область.
    write(
        &repo.join("base").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions>
  <Metadata name="Catalog.Живой" id="z1" configVersion="v2"/>
</ConfigVersions></ConfigDumpInfo>"#,
    );
    let stats = reconcile_area(&repo, conn, &sub_config_roots(&repo), &repo.join("base")).unwrap();
    assert_eq!(stats.deleted_objects, 1);

    assert_eq!(cnt1(conn, "SELECT COUNT(*) FROM metadata_objects WHERE full_name = ?", "Catalog.Удаляемый"), 0);
    assert_eq!(cnt1(conn, "SELECT COUNT(*) FROM config_manifest WHERE full_name LIKE ?", "Catalog.Удаляемый%"), 0);
    assert_eq!(cnt1(conn, "SELECT COUNT(*) FROM metadata_objects WHERE full_name = ?", "Catalog.Живой"), 1);
}

#[test]
fn reconcile_borrower_drop_keeps_object() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    // База: объект Общий (дом = база).
    write(
        &repo.join("base").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Общий</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("Catalogs").join("Общий.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Общий</Name></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Общий" id="o1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
    // Расширение EF_A заимствует Общий (Adopted).
    write(
        &repo.join("extensions").join("EF_A").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Общий</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("extensions").join("EF_A").join("Catalogs").join("Общий.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Общий</Name><ObjectBelonging>Adopted</ObjectBelonging></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("extensions").join("EF_A").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Общий" id="o1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();
    // до: объект в реестре в двух областях
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM config_manifest WHERE full_name='Catalog.Общий'", [], |r| r.get::<_, i64>(0)).unwrap(),
        2
    );

    // EF_A перестало заимствовать: пропал из его описи И его копия XML удалена с диска.
    std::fs::remove_file(repo.join("extensions").join("EF_A").join("Catalogs").join("Общий.xml")).unwrap();
    write(
        &repo.join("extensions").join("EF_A").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions></ConfigVersions></ConfigDumpInfo>"#,
    );
    let stats = reconcile_area(&repo, conn, &sub_config_roots(&repo), &repo.join("extensions").join("EF_A")).unwrap();
    assert_eq!(stats.remerged_objects, 1, "заимствователь уронил — пере-сборка, не удаление");
    assert_eq!(stats.deleted_objects, 0);

    let cnt2 = |c: &rusqlite::Connection, sql: &str, a: &str, b: &str| -> i64 {
        c.query_row(sql, params![a, b], |r| r.get::<_, i64>(0)).unwrap()
    };
    // объект цел (дом — база); участие EF_A снято, база осталась
    assert_eq!(cnt2(conn, "SELECT COUNT(*) FROM metadata_objects WHERE full_name=? AND sub_config=?", "Catalog.Общий", ""), 1);
    assert_eq!(cnt2(conn, "SELECT COUNT(*) FROM config_manifest WHERE full_name=? AND area=?", "Catalog.Общий", ""), 1);
    assert_eq!(cnt2(conn, "SELECT COUNT(*) FROM config_manifest WHERE full_name=? AND area=?", "Catalog.Общий", "extensions/EF_A"), 0);
}

#[test]
fn reconcile_subelement_disappearance_registry_only() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    write(
        &repo.join("base").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Товар</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("Catalogs").join("Товар.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Товар</Name></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions>
  <Metadata name="Catalog.Товар" id="t1" configVersion="v1">
<Metadata name="Catalog.Товар.Attribute.Цвет" id="c1"/>
  </Metadata>
</ConfigVersions></ConfigDumpInfo>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM config_manifest WHERE full_name='Catalog.Товар.Attribute.Цвет'", [], |r| r.get::<_, i64>(0)).unwrap(),
        1
    );

    // Убрали реквизит Цвет; объект Товар остался.
    write(
        &repo.join("base").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions>
  <Metadata name="Catalog.Товар" id="t1" configVersion="v1"/>
</ConfigVersions></ConfigDumpInfo>"#,
    );
    let stats = reconcile_area(&repo, conn, &sub_config_roots(&repo), &repo.join("base")).unwrap();
    assert_eq!(stats.deleted_objects, 0, "реквизит — не удаление объекта (Вариант А)");
    assert_eq!(stats.remerged_objects, 0);

    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM metadata_objects WHERE full_name='Catalog.Товар'", [], |r| r.get::<_, i64>(0)).unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM config_manifest WHERE full_name='Catalog.Товар.Attribute.Цвет'", [], |r| r.get::<_, i64>(0)).unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM config_manifest WHERE full_name='Catalog.Товар'", [], |r| r.get::<_, i64>(0)).unwrap(),
        1
    );
}

#[test]
fn incremental_dumpinfo_change_dispatches_reconcile_cascade() {
    // Фаза 3: изменение ConfigDumpInfo.xml в батче run_incremental_extras
    // маршрутизируется в reconcile_area затронутой области. Объект пропал из
    // свежей описи домашней (base) области → каскадное удаление из индекса,
    // БЕЗ опоры на Configuration.xml как триггер.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    write(
        &repo.join("base").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Удаляемый</Catalog><Catalog>Живой</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("Catalogs").join("Удаляемый.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Удаляемый</Name></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("Catalogs").join("Живой.xml"),
        r#"<MetaDataObject><Catalog><Properties><Name>Живой</Name></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions>
  <Metadata name="Catalog.Удаляемый" id="u1" configVersion="v1"/>
  <Metadata name="Catalog.Живой" id="z1" configVersion="v2"/>
</ConfigVersions></ConfigDumpInfo>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    {
        let conn = storage.conn();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM metadata_objects WHERE full_name='Catalog.Удаляемый'", [], |r| r.get::<_, i64>(0)).unwrap(),
            1
        );
    }

    // Удаляемый пропал из описи; его XML тоже удалён с диска. Батч: изменённая
    // опись (триггер сверки) + удаление объектного XML.
    write(
        &repo.join("base").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions>
  <Metadata name="Catalog.Живой" id="z1" configVersion="v2"/>
</ConfigVersions></ConfigDumpInfo>"#,
    );
    let del_xml = repo.join("base").join("Catalogs").join("Удаляемый.xml");
    std::fs::remove_file(&del_xml).unwrap();
    let dump = repo.join("base").join("ConfigDumpInfo.xml");
    run_incremental_extras(&repo, &mut storage, &[dump], &[del_xml]).unwrap();

    let conn = storage.conn();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM metadata_objects WHERE full_name='Catalog.Удаляемый'", [], |r| r.get::<_, i64>(0)).unwrap(),
        0,
        "объект каскадно удалён через диспетчеризацию в reconcile_area"
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM config_manifest WHERE full_name LIKE 'Catalog.Удаляемый%'", [], |r| r.get::<_, i64>(0)).unwrap(),
        0,
        "строки реестра удаляемого объекта убраны"
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM metadata_objects WHERE full_name='Catalog.Живой'", [], |r| r.get::<_, i64>(0)).unwrap(),
        1,
        "живой объект не задет"
    );
}

#[test]
fn incremental_bsl_event_upserts_metadata_module_matches_full() {
    // .bsl-событие БЕЗ Configuration.xml в батче (config_changed=false):
    // строку metadata_modules нового модуля заводит точечная ветка
    // update_metadata_module_for_file. Результат обязан совпасть с полным
    // run_index_extras (те же classify/owner/uuid хелперы).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?><MetaDataObject><Configuration><ChildObjects><Catalog>Склады</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("Catalogs").join("Склады.xml"),
        r#"<?xml version="1.0"?><MetaDataObject><Catalog uuid="11111111-1111-1111-1111-111111111111"><Properties><Name>Склады</Name></Properties></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("ConfigDumpInfo.xml"),
        r#"<?xml version="1.0"?><ConfigDumpInfo><ConfigVersions><Metadata id="11111111-1111-1111-1111-111111111111" configVersion="VER-1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
    let bsl = repo
        .join("Catalogs")
        .join("Склады")
        .join("Ext")
        .join("ManagerModule.bsl");
    write(&bsl, "Процедура П() Экспорт\nКонецПроцедуры");

    let module_row = |st: &Storage| -> Option<(String, String, Option<String>, String)> {
        st.conn()
            .query_row(
                "SELECT full_name, object_id, config_version, extension_name \
                 FROM metadata_modules WHERE repo=? AND module_type='ManagerModule'",
                params![REPO_DEFAULT],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()
    };

    // Эталон: полный пересбор.
    let tmp_full = TempDir::new().unwrap();
    let mut full = fresh_storage(&tmp_full);
    run_index_extras(&repo, &mut full).unwrap();
    let full_row = module_row(&full).expect("полный пересбор завёл строку модуля");
    assert_eq!(full_row.0, "Catalogs.Склады.ManagerModule");
    assert_eq!(full_row.1, "11111111-1111-1111-1111-111111111111");
    assert_eq!(full_row.2.as_deref(), Some("VER-1"));

    // Инкрементальный путь: baseline → убрали строку модуля → .bsl-событие её
    // восстанавливает через точечную ветку (Configuration.xml в батче нет).
    let tmp_inc = TempDir::new().unwrap();
    let mut inc = fresh_storage(&tmp_inc);
    run_index_extras(&repo, &mut inc).unwrap();
    inc.conn()
        .execute(
            "DELETE FROM metadata_modules WHERE repo=? AND module_type='ManagerModule'",
            params![REPO_DEFAULT],
        )
        .unwrap();
    assert!(module_row(&inc).is_none(), "строку убрали для чистоты теста");

    run_incremental_extras(&repo, &mut inc, &[bsl.clone()], &[]).unwrap();
    let inc_row = module_row(&inc).expect("точечная ветка восстановила строку");
    assert_eq!(inc_row, full_row, "инкрементальный upsert модуля == полный пересбор");
}

#[test]
fn stats_drifted_threshold_is_1_5x_with_floor() {
    // Пол: мелкие таблицы (обе величины < 1000) не триггерят, даже кратно.
    assert!(!stats_drifted(30, Some(10)), "3×, но обе < FLOOR");
    assert!(!stats_drifted(500, None), "нет статы, но таблица < FLOOR");
    // Рост крупной таблицы: 1.5× ровно — дрейф, 1.33× — нет.
    assert!(stats_drifted(9000, Some(6000)), "1.5× ровно");
    assert!(!stats_drifted(8000, Some(6000)), "1.33× — не дрейф");
    // Срез: до /1.5 — дрейф, 0.75× — нет.
    assert!(stats_drifted(4000, Some(6000)), "6000/1.5 = 4000 — дрейф вниз");
    assert!(!stats_drifted(4500, Some(6000)), "0.75× — не дрейф");
    // Нет статистики, но таблица уже крупная.
    assert!(stats_drifted(5000, None));
}

#[test]
fn maybe_analyze_runs_when_graph_grows() {
    let tmp = TempDir::new().unwrap();
    let storage = fresh_storage(&tmp);
    let conn = storage.conn();
    // Наполняем data_links выше пола; статистики ещё нет.
    for i in 0..1500 {
        conn.execute(
            "INSERT OR IGNORE INTO data_links \
             (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
             VALUES (?, 'Catalog.A', ?, ?, 'attr', 0, 0)",
            params![REPO_DEFAULT, format!("p{}", i), format!("Catalog.T{}", i)],
        )
        .unwrap();
    }
    assert!(analyzed_row_count(conn, "data_links").is_none(), "до ANALYZE статы нет");

    maybe_analyze_graph_tables(conn).unwrap();

    let rec = analyzed_row_count(conn, "data_links");
    assert!(rec.is_some(), "ANALYZE должен был записать sqlite_stat1");
    assert!(rec.unwrap() >= 1000, "записанное число строк отражает реальный размер");
}

#[test]
fn idempotent_repeated_runs_dont_dupe() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Catalog>X</Catalog>
</ChildObjects></Configuration></MetaDataObject>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    run_index_extras(&repo, &mut storage).unwrap();
    run_index_extras(&repo, &mut storage).unwrap();

    let count: i64 = storage
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM metadata_objects WHERE repo = ?",
            params![REPO_DEFAULT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "повторный run не должен плодить дубликаты");
}

#[test]
fn extras_present_requires_meta_and_terms() {
    use crate::processor::BslLanguageProcessor;
    use code_index_core::extension::processor::LanguageProcessor;

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects><Catalog>X</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    let mut storage = fresh_storage(&tmp);
    let proc = BslLanguageProcessor::new();

    // 1. Свежая БД — extras пусты → false (демон сделает полный проход).
    assert!(!proc.extras_present(&storage), "пустые extras → false");

    // 2. metadata_objects наполнено, но .bsl нет → terms пусты → всё ещё false
    //    (гейт требует ОБЕ ключевые таблицы непустыми).
    run_index_extras(&repo, &mut storage).unwrap();
    assert!(
        !proc.extras_present(&storage),
        "metadata без механических terms → false"
    );

    // 3. Добавили механический терм → обе таблицы непусты → true
    //    (рестарт демона при неизменных данных может пропустить пересбор).
    storage
        .conn()
        .execute(
            "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
             VALUES (?1, 'X.bsl::П', 'термин', 'mech:v1', 0)",
            params![REPO_DEFAULT],
        )
        .unwrap();
    assert!(
        proc.extras_present(&storage),
        "metadata_objects + mech-terms непусты → true"
    );
}

/// Мини-репо для тестов механических термов: общий модуль с синонимом
/// и процедурой с комментарием. files/functions заполняются вручную
/// (как будто core-парсер уже отработал — extras его не запускают).
fn write_terms_fixture(repo: &Path, storage: &Storage) {
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <CommonModule>РаботаСоШтрихкодами</CommonModule>
</ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("CommonModules").join("РаботаСоШтрихкодами.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <CommonModule>
<Properties>
  <Name>РаботаСоШтрихкодами</Name>
  <Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Работа со штрихкодами</v8:content></v8:item></Synonym>
</Properties>
  </CommonModule>
</MetaDataObject>"#,
    );
    write(
        &repo
            .join("CommonModules")
            .join("РаботаСоШтрихкодами")
            .join("Ext")
            .join("Module.bsl"),
        "// Уточняет данные номенклатуры по штрихкоду.\n\
         &НаСервере\n\
         Процедура УточнитьДанныеПоШтрихкоду() Экспорт\n\
         КонецПроцедуры\n",
    );
    let conn = storage.conn();
    conn.execute(
        "INSERT INTO files (path, content_hash, language) \
         VALUES ('CommonModules/РаботаСоШтрихкодами/Ext/Module.bsl', 'h', 'bsl')",
        [],
    )
    .unwrap();
    let fid: i64 = conn
        .query_row("SELECT id FROM files WHERE language='bsl'", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO functions (file_id, name, line_start) \
         VALUES (?, 'УточнитьДанныеПоШтрихкоду', 3)",
        params![fid],
    )
    .unwrap();
}

#[test]
fn mechanical_terms_include_name_synonym_and_comment() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut storage = fresh_storage(&tmp);
    write_terms_fixture(&repo, &storage);

    run_index_extras(&repo, &mut storage).unwrap();

    let (terms, sig): (String, String) = storage
        .conn()
        .query_row(
            "SELECT terms, signature FROM procedure_enrichment \
             WHERE repo = ?1 AND proc_key = ?2",
            params![
                REPO_DEFAULT,
                "CommonModules/РаботаСоШтрихкодами/Ext/Module.bsl::УточнитьДанныеПоШтрихкоду"
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(terms.contains("уточнить данные по штрихкоду"), "слова имени: {terms}");
    assert!(terms.contains("работа со штрихкодами"), "синоним объекта: {terms}");
    assert!(terms.contains("уточняет данные номенклатуры"), "комментарий: {terms}");
    assert_eq!(sig, crate::terms::MECH_SIGNATURE);

    // FTS (trigram): словоформа и подстрока находят процедуру.
    for q in ["штрихкод", "уточн", "работа со штрихкодами"] {
        let hits: i64 = storage
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM fts_procedure_enrichment WHERE terms MATCH ?1",
                params![q],
                |r| r.get(0),
            )
            .unwrap();
        assert!(hits >= 1, "FTS должен находить '{q}'");
    }
}

#[test]
fn mechanical_terms_dont_touch_llm_rows() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut storage = fresh_storage(&tmp);
    write_terms_fixture(&repo, &storage);
    // Существующая LLM-запись той же процедуры.
    storage
        .conn()
        .execute(
            "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
             VALUES (?1, ?2, 'llm-термины, бережно сохранить', 'openai_compatible:m', 1)",
            params![
                REPO_DEFAULT,
                "CommonModules/РаботаСоШтрихкодами/Ext/Module.bsl::УточнитьДанныеПоШтрихкоду"
            ],
        )
        .unwrap();

    run_index_extras(&repo, &mut storage).unwrap();

    let (terms, sig): (String, String) = storage
        .conn()
        .query_row(
            "SELECT terms, signature FROM procedure_enrichment \
             WHERE repo = ?1 AND proc_key = ?2",
            params![
                REPO_DEFAULT,
                "CommonModules/РаботаСоШтрихкодами/Ext/Module.bsl::УточнитьДанныеПоШтрихкоду"
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(terms, "llm-термины, бережно сохранить", "LLM-строка не перетёрта");
    assert_eq!(sig, "openai_compatible:m");
}

#[test]
fn incremental_terms_update_and_cleanup() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut storage = fresh_storage(&tmp);
    write_terms_fixture(&repo, &storage);
    run_index_extras(&repo, &mut storage).unwrap();

    let bsl_abs = repo
        .join("CommonModules")
        .join("РаботаСоШтрихкодами")
        .join("Ext")
        .join("Module.bsl");

    // Файл «изменился»: добавилась процедура (в functions и на диске).
    write(
        &bsl_abs,
        "// Уточняет данные номенклатуры по штрихкоду.\n\
         &НаСервере\n\
         Процедура УточнитьДанныеПоШтрихкоду() Экспорт\n\
         КонецПроцедуры\n\
         \n\
         // Печатает этикетку со штрихкодом.\n\
         Процедура НапечататьЭтикетку() Экспорт\n\
         КонецПроцедуры\n",
    );
    {
        let conn = storage.conn();
        let fid: i64 = conn
            .query_row("SELECT id FROM files WHERE language='bsl'", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO functions (file_id, name, line_start) \
             VALUES (?, 'НапечататьЭтикетку', 7)",
            params![fid],
        )
        .unwrap();
    }
    run_incremental_extras(&repo, &mut storage, &[bsl_abs.clone()], &[]).unwrap();

    let count: i64 = storage
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM procedure_enrichment WHERE repo = ?1 AND signature LIKE 'mech:%'",
            params![REPO_DEFAULT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "после инкремента — термы обеих процедур");
    let terms: String = storage
        .conn()
        .query_row(
            "SELECT terms FROM procedure_enrichment WHERE repo = ?1 AND proc_key LIKE '%НапечататьЭтикетку'",
            params![REPO_DEFAULT],
            |r| r.get(0),
        )
        .unwrap();
    assert!(terms.contains("напечатать этикетку"), "{terms}");
    assert!(terms.contains("печатает этикетку"), "{terms}");

    // Файл удалён → mech-строки файла зачищены.
    std::fs::remove_file(&bsl_abs).unwrap();
    run_incremental_extras(&repo, &mut storage, &[], &[bsl_abs]).unwrap();
    let after: i64 = storage
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM procedure_enrichment WHERE repo = ?1 AND signature LIKE 'mech:%'",
            params![REPO_DEFAULT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, 0, "после удаления файла mech-строки зачищены");
}

#[test]
fn fills_event_subscriptions() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("EventSubscriptions").join("MySub.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject>
  <EventSubscription>
<Properties>
  <Name>MySub</Name>
  <Source><Type><v8:Type>cfg:DocumentRef.X</v8:Type></Type></Source>
  <Event>ПриЗаписи</Event>
  <Handler>МойМодуль.МойОбработчик</Handler>
</Properties>
  </EventSubscription>
</MetaDataObject>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();

    let row: (String, String, String) = storage
        .conn()
        .query_row(
            "SELECT name, handler_module, handler_proc FROM event_subscriptions WHERE repo = ?",
            params![REPO_DEFAULT],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("MySub".into(), "МойМодуль".into(), "МойОбработчик".into()));
}

#[test]
fn call_graph_includes_extension_override() {
    // Перехват &Вместо ПробитьЧек в расширении → ребро extension_override
    // ПробитьЧек → EEРМК_ПробитьЧек. Источник — functions.override_*.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><Properties><Name>C</Name></Properties></Configuration></MetaDataObject>"#,
    );
    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();
    // Перехватчик в functions (как будто его распарсил core-парсер из CFE).
    conn.execute(
        "INSERT INTO files (path, content_hash, language) \
         VALUES ('extensions/E/Documents/X/Ext/Form/Module.bsl', 'h', 'bsl')",
        [],
    )
    .unwrap();
    let fid: i64 = conn
        .query_row("SELECT id FROM files WHERE path LIKE '%Module.bsl'", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO functions (file_id, name, override_type, override_target) \
         VALUES (?, 'EEРМК_ПробитьЧек', 'Вместо', 'ПробитьЧек')",
        params![fid],
    )
    .unwrap();
    build_call_graph(conn).unwrap();
    let cnt: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM proc_call_graph \
             WHERE call_type = 'extension_override' \
               AND caller_proc_key = 'ПробитьЧек' AND callee_proc_name = 'EEРМК_ПробитьЧек'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt, 1, "должно появиться ребро перехвата extension_override");

    // Инкрементальный rebuild идемпотентен (не дублирует ребро).
    rebuild_call_graph_extension_override(conn).unwrap();
    let cnt2: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM proc_call_graph WHERE call_type = 'extension_override'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt2, 1, "rebuild не должен дублировать ребро");
}

#[test]
fn call_graph_combines_subscriptions_and_form_events() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    // EventSubscription
    write(
        &repo.join("EventSubscriptions").join("Sub.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject>
  <EventSubscription>
<Properties>
  <Name>Sub</Name>
  <Source><Type><v8:Type>cfg:DocumentRef.X</v8:Type></Type></Source>
  <Event>ПриЗаписи</Event>
  <Handler>М.П</Handler>
</Properties>
  </EventSubscription>
</MetaDataObject>"#,
    );
    // Form
    write(
        &repo
            .join("Documents")
            .join("X")
            .join("Forms")
            .join("Ф")
            .join("Ext")
            .join("Form.xml"),
        r#"<?xml version="1.0"?>
<Form><Events>
  <Event name="ПриОткрытии">ПриОткрытии</Event>
</Events></Form>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();

    let by_type: Vec<(String, i64)> = conn
        .prepare("SELECT call_type, COUNT(*) FROM proc_call_graph GROUP BY call_type")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let map: std::collections::HashMap<String, i64> = by_type.into_iter().collect();
    assert_eq!(
        map.get("subscription").copied(),
        Some(1),
        "одна подписка"
    );
    assert_eq!(
        map.get("form_event").copied(),
        Some(1),
        "один обработчик формы"
    );
    // direct рёбер не должно быть — `calls` core пуст (нет .bsl-кода).
    assert!(map.get("direct").copied().unwrap_or(0) == 0);
}

#[test]
fn fills_metadata_forms_from_dump_layout() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    // Реалистичный layout DumpConfigToFiles:
    //   Documents/Реализация/Forms/ФормаДокумента/Ext/Form.xml
    let form_path = repo
        .join("Documents")
        .join("Реализация")
        .join("Forms")
        .join("ФормаДокумента")
        .join("Ext")
        .join("Form.xml");
    write(
        &form_path,
        r#"<?xml version="1.0"?>
<Form>
  <Events>
<Event name="ПриОткрытии">ПриОткрытии</Event>
  </Events>
</Form>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();

    let row: (String, String, String) = storage
        .conn()
        .query_row(
            "SELECT owner_full_name, form_name, handlers_json FROM metadata_forms WHERE repo = ?",
            params![REPO_DEFAULT],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row.0, "Documents.Реализация");
    assert_eq!(row.1, "ФормаДокумента");
    assert!(row.2.contains("ПриОткрытии"));
}

/// Вложенная подсистема должна попадать в реестр объектов (в
/// Configuration.xml её нет — там только верхний уровень), а цель состава,
/// записанная идентификатором (так выгружаются заимствованные объекты в
/// расширениях), — разворачиваться в имя по описи выгрузки.
#[test]
fn nested_subsystem_registered_and_identifier_target_resolved() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Subsystem>Продажи</Subsystem>
  <Catalog>Контрагенты</Catalog>
</ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("Catalogs").join("Контрагенты.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Catalog uuid="aaaaaaaa-1111-2222-3333-444444444444">
<Properties><Name>Контрагенты</Name></Properties><ChildObjects/></Catalog></MetaDataObject>"#,
    );
    write(
        &repo.join("Subsystems").join("Продажи.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject xmlns:xr="x" xmlns:xsi="y"><Subsystem><Properties>
  <Name>Продажи</Name>
  <Content><xr:Item xsi:type="xr:MDObjectRef">Catalog.Контрагенты</xr:Item></Content>
</Properties><ChildObjects/></Subsystem></MetaDataObject>"#,
    );
    // Вложенная подсистема: в Configuration.xml не перечислена, цель состава
    // указана идентификатором.
    write(
        &repo
            .join("Subsystems")
            .join("Продажи")
            .join("Subsystems")
            .join("Розница.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject xmlns:xr="x" xmlns:xsi="y"><Subsystem><Properties>
  <Name>Розница</Name>
  <Content><xr:Item xsi:type="xr:MDObjectRef">aaaaaaaa-1111-2222-3333-444444444444</xr:Item></Content>
</Properties><ChildObjects/></Subsystem></MetaDataObject>"#,
    );
    write(
        &repo.join("ConfigDumpInfo.xml"),
        r#"<?xml version="1.0"?>
<ConfigDumpInfo configVersion="v0">
  <Metadata name="Catalog.Контрагенты" id="aaaaaaaa-1111-2222-3333-444444444444" configVersion="v1"/>
</ConfigDumpInfo>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();

    let nested: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM metadata_objects WHERE full_name='Subsystem.Розница'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(nested, 1, "вложенная подсистема должна быть в реестре объектов");

    let resolved: String = conn
        .query_row(
            "SELECT to_object FROM data_links WHERE link_kind='subsystem_content' \
             AND from_object='Subsystem.Розница'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        resolved, "Catalog.Контрагенты",
        "идентификатор в составе подсистемы должен разворачиваться в имя объекта"
    );
}

/// Создать фикстуру конфигурации с источниками конфиг-уровня и ролью.
fn write_config_level_fixture(repo: &Path) {
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects/></Configuration></MetaDataObject>"#,
    );
    // Подсистема с составом (2 объекта).
    write(
        &repo.join("Subsystems").join("Продажи.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject xmlns:xr="x" xmlns:xsi="y"><Subsystem><Properties>
  <Name>Продажи</Name>
  <Content>
<xr:Item xsi:type="xr:MDObjectRef">Document.РеализацияТоваровУслуг</xr:Item>
<xr:Item xsi:type="xr:MDObjectRef">Catalog.Контрагенты</xr:Item>
  </Content>
</Properties><ChildObjects/></Subsystem></MetaDataObject>"#,
    );
    // План обмена: Content.xml.
    write(
        &repo.join("ExchangePlans").join("Обмен").join("Ext").join("Content.xml"),
        r#"<?xml version="1.0"?>
<ExchangePlanContent xmlns="z">
  <Item><Metadata>Catalog.Номенклатура</Metadata><AutoRecord>Deny</AutoRecord></Item>
</ExchangePlanContent>"#,
    );
    // Определяемый тип: составной (2 ссылочных, 1 примитив отброшен).
    write(
        &repo.join("DefinedTypes").join("Адресат.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="c"><DefinedType><Properties><Name>Адресат</Name>
  <Type>
<v8:Type>cfg:CatalogRef.Пользователи</v8:Type>
<v8:Type>cfg:EnumRef.ВидыДат</v8:Type>
<v8:Type>xs:string</v8:Type>
  </Type>
</Properties></DefinedType></MetaDataObject>"#,
    );
    // Функциональная опция: Location в ресурс регистра.
    write(
        &repo.join("FunctionalOptions").join("ФО.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><FunctionalOption><Properties><Name>ФО</Name>
  <Location>InformationRegister.Настройки.Resource.Значение</Location>
  <Content/></Properties></FunctionalOption></MetaDataObject>"#,
    );
    // Роль: Read=true и Posting=false на документе.
    write(
        &repo.join("Roles").join("Роль1").join("Ext").join("Rights.xml"),
        r#"<?xml version="1.0"?>
<Rights xmlns="r"><object>
  <name>Document.РеализацияТоваровУслуг</name>
  <right><name>Read</name><value>true</value></right>
  <right><name>Posting</name><value>false</value></right>
</object></Rights>"#,
    );
}

#[test]
fn fills_metadata_refs_and_role_rights() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write_config_level_fixture(&repo);

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();

    // subsystem_content: 2 ребра, from_object = Subsystem.Продажи.
    let subs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM data_links WHERE link_kind='subsystem_content' \
             AND from_object='Subsystem.Продажи'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(subs, 2, "subsystem_content");

    // exchange_plan_content: ExchangePlan.Обмен → Catalog.Номенклатура.
    let ep: String = conn
        .query_row(
            "SELECT to_object FROM data_links WHERE link_kind='exchange_plan_content' \
             AND from_object='ExchangePlan.Обмен'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ep, "Catalog.Номенклатура");

    // defined_type_content: 2 ссылочных, is_composite=1, примитив отброшен.
    let (dt_cnt, dt_comp): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), MAX(is_composite) FROM data_links \
             WHERE link_kind='defined_type_content' AND from_object='DefinedType.Адресат'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(dt_cnt, 2, "defined_type_content edges");
    assert_eq!(dt_comp, 1, "defined_type_content is_composite");

    // functional_option_location: FunctionalOption.ФО → InformationRegister.Настройки.
    let (fo_to, fo_path): (String, String) = conn
        .query_row(
            "SELECT to_object, from_path FROM data_links \
             WHERE link_kind='functional_option_location' AND from_object='FunctionalOption.ФО'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(fo_to, "InformationRegister.Настройки");
    assert!(fo_path.ends_with("Resource.Значение"));

    // role_rights: только granted (Read), Posting=false отброшен.
    let rr: Vec<(String, String, String)> = {
        let mut s = conn
            .prepare("SELECT role_name, object_name, right_name FROM role_rights ORDER BY right_name")
            .unwrap();
        let rows = s
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.map(|x| x.unwrap()).collect()
    };
    assert_eq!(
        rr,
        vec![(
            "Роль1".to_string(),
            "Document.РеализацияТоваровУслуг".to_string(),
            "Read".to_string()
        )]
    );

    // Идемпотентность: повторный полный прогон не плодит дубли.
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM data_links WHERE link_kind IN \
             ('subsystem_content','exchange_plan_content','defined_type_content','functional_option_location')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 2 + 1 + 2 + 1, "config-level data_links после повтора");
    let rr_total: i64 = conn
        .query_row("SELECT COUNT(*) FROM role_rights", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rr_total, 1, "role_rights после повтора");
}

#[test]
fn incremental_rebuilds_metadata_refs_and_role_rights() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write_config_level_fixture(&repo);

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();

    // Снимок «эталона» — полный пересбор отдельной свежей БД.
    let cnt = |st: &mut Storage, sql: &str| -> i64 {
        st.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    };
    let dl_sql = "SELECT COUNT(*) FROM data_links WHERE link_kind IN \
         ('subsystem_content','exchange_plan_content','defined_type_content','functional_option_location')";
    let rr_sql = "SELECT COUNT(*) FROM role_rights";

    // Меняем состав подсистемы (добавили объект) и право роли (добавили Posting=true).
    write(
        &repo.join("Subsystems").join("Продажи.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject xmlns:xr="x" xmlns:xsi="y"><Subsystem><Properties>
  <Name>Продажи</Name>
  <Content>
<xr:Item xsi:type="xr:MDObjectRef">Document.РеализацияТоваровУслуг</xr:Item>
<xr:Item xsi:type="xr:MDObjectRef">Catalog.Контрагенты</xr:Item>
<xr:Item xsi:type="xr:MDObjectRef">Catalog.Склады</xr:Item>
  </Content>
</Properties><ChildObjects/></Subsystem></MetaDataObject>"#,
    );
    write(
        &repo.join("Roles").join("Роль1").join("Ext").join("Rights.xml"),
        r#"<?xml version="1.0"?>
<Rights xmlns="r"><object>
  <name>Document.РеализацияТоваровУслуг</name>
  <right><name>Read</name><value>true</value></right>
  <right><name>Posting</name><value>true</value></right>
</object></Rights>"#,
    );

    let changed = vec![
        repo.join("Subsystems").join("Продажи.xml"),
        repo.join("Roles").join("Роль1").join("Ext").join("Rights.xml"),
    ];
    run_incremental_extras(&repo, &mut storage, &changed, &[]).unwrap();

    // Инкремент должен совпасть с полным пересбором с нуля — отдельная БД,
    // тот же (уже изменённый) репо.
    let tmp2 = TempDir::new().unwrap();
    let mut full = fresh_storage(&tmp2);
    run_index_extras(&repo, &mut full).unwrap();

    assert_eq!(cnt(&mut storage, dl_sql), 3 + 1 + 2 + 1, "data_links после инкремента");
    assert_eq!(cnt(&mut storage, rr_sql), 2, "role_rights после инкремента");
    assert_eq!(
        cnt(&mut storage, dl_sql),
        cnt(&mut full, dl_sql),
        "config data_links: инкремент != полный пересбор"
    );
    assert_eq!(
        cnt(&mut storage, rr_sql),
        cnt(&mut full, rr_sql),
        "role_rights: инкремент != полный пересбор"
    );
}

#[test]
fn fills_metadata_code_usages_from_bsl() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects/></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("CommonModules").join("М").join("Ext").join("Module.bsl"),
        "Процедура П()\n\tДок = Документы.РеализацияТоваровУслуг.СоздатьДокумент();\n\tТекст = \"ВЫБРАТЬ Ссылка ИЗ Документ.Заказ.Товары\";\nКонецПроцедуры",
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();
    let rows: Vec<(String, Option<String>, String, i64)> = {
        let mut s = conn
            .prepare("SELECT object_ref, member_path, usage_kind, line FROM metadata_code_usages ORDER BY line")
            .unwrap();
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|x| x.unwrap())
            .collect()
    };
    assert_eq!(
        rows,
        vec![
            ("Document.РеализацияТоваровУслуг".to_string(), None, "manager".to_string(), 2),
            ("Document.Заказ".to_string(), Some("Товары".to_string()), "query".to_string(), 3),
        ]
    );

    // file_path записан относительным с forward slash.
    let fp: String = conn
        .query_row("SELECT DISTINCT file_path FROM metadata_code_usages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fp, "CommonModules/М/Ext/Module.bsl");

    // Идемпотентность: повторный прогон не плодит дубли.
    run_index_extras(&repo, &mut storage).unwrap();
    let cnt: i64 = storage
        .conn()
        .query_row("SELECT COUNT(*) FROM metadata_code_usages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cnt, 2);
}

#[test]
fn fills_data_links_from_object_xml() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    // Configuration.xml нужен, чтобы index_data_links нашёл sub-root.
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Document>РеализацияТоваровУслуг</Document>
</ChildObjects></Configuration></MetaDataObject>"#,
    );
    // Объектный XML документа: реквизит шапки + ТЧ + примитив.
    write(
        &repo.join("Documents").join("РеализацияТоваровУслуг.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Document uuid="root">
<Properties><Name>РеализацияТоваровУслуг</Name></Properties>
<ChildObjects>
  <Attribute uuid="a1"><Properties><Name>Контрагент</Name>
    <Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
  </Properties></Attribute>
  <Attribute uuid="a2"><Properties><Name>Сумма</Name>
    <Type><v8:Type>xs:decimal</v8:Type></Type>
  </Properties></Attribute>
  <TabularSection uuid="ts1"><Properties><Name>Товары</Name></Properties>
    <ChildObjects>
      <Attribute uuid="a3"><Properties><Name>Номенклатура</Name>
        <Type><v8:Type>cfg:CatalogRef.Номенклатура</v8:Type></Type>
      </Properties></Attribute>
    </ChildObjects>
  </TabularSection>
</ChildObjects>
  </Document>
</MetaDataObject>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    let conn = storage.conn();

    // Контрагент (attr) + Товары.Номенклатура (tabular_attr) = 2 ребра.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM data_links WHERE repo = ?", params![REPO_DEFAULT], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2, "ожидаем 2 ссылочных ребра (примитив Сумма пропущен)");

    let (from_path, to_object, kind): (String, String, String) = conn
        .query_row(
            "SELECT from_path, to_object, link_kind FROM data_links \
             WHERE repo = ? AND from_object = 'Document.РеализацияТоваровУслуг' AND from_path = 'Контрагент'",
            params![REPO_DEFAULT],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(from_path, "Контрагент");
    assert_eq!(to_object, "Catalog.Контрагенты");
    assert_eq!(kind, "attr");

    // Реквизит табличной части.
    let tab_to: String = conn
        .query_row(
            "SELECT to_object FROM data_links WHERE repo = ? AND from_path = 'Товары.Номенклатура'",
            params![REPO_DEFAULT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tab_to, "Catalog.Номенклатура");
}

#[test]
fn data_links_idempotent() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write(
        &repo.join("Configuration.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Catalog>Тест</Catalog>
</ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("Catalogs").join("Тест.xml"),
        r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root"><ChildObjects>
<Attribute uuid="a1"><Properties><Name>Владелец</Name>
  <Type><v8:Type>cfg:CatalogRef.Организации</v8:Type></Type>
</Properties></Attribute>
  </ChildObjects></Catalog>
</MetaDataObject>"#,
    );

    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();
    run_index_extras(&repo, &mut storage).unwrap();
    run_index_extras(&repo, &mut storage).unwrap();

    let count: i64 = storage
        .conn()
        .query_row("SELECT COUNT(*) FROM data_links WHERE repo = ?", params![REPO_DEFAULT], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "повторный run не должен плодить дубликаты рёбер");
}

// ── Эквивалентность инкрементального обновления полному пересбору ──────
//
// Главный приёмочный тест варианта A: после правки одного файла +
// run_incremental_extras итоговые таблицы должны совпасть с полным
// run_index_extras на той же конечной версии репо.

fn snapshot_pcg(conn: &rusqlite::Connection) -> Vec<(String, String, String)> {
    let mut v: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT caller_proc_key, callee_proc_name, call_type \
             FROM proc_call_graph WHERE repo = ?",
        )
        .unwrap()
        .query_map(params![REPO_DEFAULT], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    v.sort();
    v
}

fn snapshot_dl(conn: &rusqlite::Connection) -> Vec<(String, String, String, String)> {
    let mut v: Vec<(String, String, String, String)> = conn
        .prepare(
            "SELECT from_object, from_path, to_object, link_kind \
             FROM data_links WHERE repo = ?",
        )
        .unwrap()
        .query_map(params![REPO_DEFAULT], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    v.sort();
    v
}

fn snapshot_attrs(conn: &rusqlite::Connection) -> Vec<(String, Option<String>)> {
    let mut v: Vec<(String, Option<String>)> = conn
        .prepare("SELECT full_name, attributes_json FROM metadata_objects WHERE repo = ?")
        .unwrap()
        .query_map(params![REPO_DEFAULT], |r| {
            Ok((r.get(0)?, r.get::<_, Option<String>>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    v.sort();
    v
}

fn ensure_file(conn: &rusqlite::Connection, path: &str) -> i64 {
    conn.execute(
        "INSERT OR IGNORE INTO files (path, content_hash, language) VALUES (?, 'h', 'bsl')",
        params![path],
    )
    .unwrap();
    conn.query_row("SELECT id FROM files WHERE path = ?", params![path], |r| r.get(0))
        .unwrap()
}

fn set_calls(conn: &rusqlite::Connection, file_id: i64, edges: &[(&str, &str)]) {
    conn.execute("DELETE FROM calls WHERE file_id = ?", params![file_id])
        .unwrap();
    for (caller, callee) in edges {
        conn.execute(
            "INSERT INTO calls (file_id, caller, callee, line) VALUES (?, ?, ?, 1)",
            params![file_id, caller, callee],
        )
        .unwrap();
    }
}

fn set_func(conn: &rusqlite::Connection, file_id: i64, name: &str, args: &str) {
    conn.execute(
        "INSERT INTO functions (file_id, name, args) VALUES (?, ?, ?)",
        params![file_id, name, args],
    )
    .unwrap();
}

#[test]
fn resolves_callee_keys_local_unique_export_and_null() {
    // Этап 4e: проверяем оба tier'а резолвера и честный NULL.
    let tmp = TempDir::new().unwrap();
    let st = fresh_storage(&tmp);
    let conn = st.conn();

    let p1 = "Documents/Реализация/Ext/ObjectModule.bsl";
    let p2 = "Documents/Поступление/Ext/ObjectModule.bsl";
    let util = "CommonModules/Util/Ext/Module.bsl";
    let a = "CommonModules/A/Ext/Module.bsl";
    let b = "CommonModules/B/Ext/Module.bsl";
    let f1 = ensure_file(conn, p1);
    let f2 = ensure_file(conn, p2);
    let fu = ensure_file(conn, util);
    let fa = ensure_file(conn, a);
    let fb = ensure_file(conn, b);

    // Процедуры: локальные (без Экспорт) + экспортные ('() Экспорт').
    set_func(conn, f1, "ОбработкаПроведения", "()");
    set_func(conn, f1, "МестныйПомощник", "()");
    set_func(conn, f2, "ОбработкаПроведения", "()");
    set_func(conn, fu, "ОбщийУникальный", "() Экспорт");
    set_func(conn, fa, "Дубликат", "() Экспорт");
    set_func(conn, fb, "Дубликат", "() Экспорт");

    set_calls(
        conn,
        f1,
        &[
            ("ОбработкаПроведения", "МестныйПомощник"), // локальный → резолв в p1
            ("ОбработкаПроведения", "ОбщийУникальный"), // уникальный экспорт → util
            ("ОбработкаПроведения", "Дубликат"),       // неоднозначный экспорт → NULL
            ("ОбработкаПроведения", "ВнешнийНеизвестный"), // не резолвится, не балласт → NULL
        ],
    );
    set_calls(conn, f2, &[("ОбработкаПроведения", "ДругойМетод")]);

    build_call_graph(conn).unwrap();

    // 1) Одноимённые caller разведены по файлам (Шаг 1).
    let callers: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT caller_proc_key FROM proc_call_graph \
             WHERE call_type='direct' ORDER BY caller_proc_key",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        callers.contains(&format!("{p1}::ОбработкаПроведения")),
        "caller из p1 несёт путь: {callers:?}"
    );
    assert!(
        callers.contains(&format!("{p2}::ОбработкаПроведения")),
        "caller из p2 несёт путь — одноимённые НЕ схлопнуты"
    );

    // callee_proc_key для ребра из p1 по имени callee.
    let key = |callee: &str| -> Option<String> {
        conn.query_row(
            "SELECT callee_proc_key FROM proc_call_graph \
             WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
            params![format!("{p1}::ОбработкаПроведения"), callee],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
    };

    // 2) Локальный вызов → адрес в файле вызывателя.
    assert_eq!(key("МестныйПомощник"), Some(format!("{p1}::МестныйПомощник")));
    // 3) Уникальный экспорт → адрес единственного носителя.
    assert_eq!(key("ОбщийУникальный"), Some(format!("{util}::ОбщийУникальный")));
    // 4) Неоднозначный экспорт (2 модуля) → честный NULL.
    assert_eq!(key("Дубликат"), None, "неоднозначный экспорт не должен привязываться");
    // 5) Нерезолвимое имя (нет такой процедуры, не балласт) → NULL, ребро на месте.
    assert_eq!(key("ВнешнийНеизвестный"), None, "нерезолвимый вызов не привязывается");
}

#[test]
fn prunes_platform_balast_keeps_real_and_resolved() {
    // Этап 4e-prune: балластное ребро (callee_proc_key NULL) удаляется;
    // реальное локальное ребро остаётся; имя из списка балласта, которое
    // РЕЗОЛВИЛОСЬ в реальную процедуру (callee_proc_key != NULL), сохраняется
    // (защита от коллизий имён по IS NULL).
    let tmp = TempDir::new().unwrap();
    let st = fresh_storage(&tmp);
    let conn = st.conn();

    let p1 = "Documents/Реализация/Ext/ObjectModule.bsl";
    let util = "CommonModules/Util/Ext/Module.bsl";
    let mod_c = "CommonModules/C/Ext/Module.bsl";
    let mod_d = "CommonModules/D/Ext/Module.bsl";
    let f1 = ensure_file(conn, p1);
    let fu = ensure_file(conn, util);
    let fc = ensure_file(conn, mod_c);
    let fd = ensure_file(conn, mod_d);

    set_func(conn, f1, "ОбработкаПроведения", "()");
    set_func(conn, f1, "МестныйПомощник", "()");
    // Экспортная процедура с именем, СОВПАДАЮЩИМ с балластным ("Найти"), уникальна.
    set_func(conn, fu, "Найти", "() Экспорт");
    // Балластное имя "Записать", экспортное НЕОДНОЗНАЧНО (2 модуля) → не резолвится.
    set_func(conn, fc, "Записать", "() Экспорт");
    set_func(conn, fd, "Записать", "() Экспорт");

    set_calls(
        conn,
        f1,
        &[
            ("ОбработкаПроведения", "Добавить"),        // балласт, не экспорт, не резолв → удалить
            ("ОбработкаПроведения", "МестныйПомощник"), // реальное локальное → оставить
            ("ОбработкаПроведения", "Найти"),           // балластное ИМЯ, но резолвится → оставить
            ("ОбработкаПроведения", "Записать"),        // балласт + экспорт-коллизия, NULL → оставить
        ],
    );

    build_call_graph(conn).unwrap();

    let exists = |callee: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM proc_call_graph \
             WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
            params![format!("{p1}::ОбработкаПроведения"), callee],
            |r| r.get(0),
        )
        .unwrap()
    };

    assert_eq!(exists("Добавить"), 0, "балластное ребро (не экспорт, не резолв) удаляется");
    assert_eq!(exists("МестныйПомощник"), 1, "реальное локальное ребро остаётся");
    assert_eq!(
        exists("Найти"),
        1,
        "балластное ИМЯ, резолвленное в реальную процедуру, сохраняется (IS NULL guard)"
    );
    assert_eq!(
        exists("Записать"),
        1,
        "балластное ИМЯ, экспортное в конфиге неоднозначно (NULL), не отсеивается (collision guard)"
    );
}

#[test]
fn resolves_callee_key_by_module_qualifier() {
    // Tier C (CORE B): склеенный вызов `Модуль.Метод` резолвится точно по
    // квалификатору общего модуля — даже если имя метода экспортно в ≥2
    // модулях (что Tier B оставлял бы честным NULL).
    let tmp = TempDir::new().unwrap();
    let st = fresh_storage(&tmp);
    let conn = st.conn();

    let caller = "Documents/Реализация/Ext/ObjectModule.bsl";
    let mod_a = "base/CommonModules/МодульА/Ext/Module.bsl";
    let mod_b = "base/CommonModules/МодульБ/Ext/Module.bsl";
    let fc = ensure_file(conn, caller);
    let fa = ensure_file(conn, mod_a);
    let fb = ensure_file(conn, mod_b);

    set_func(conn, fc, "ОбработкаПроведения", "()");
    // Одно и то же имя метода экспортно в ДВУХ общих модулях.
    set_func(conn, fa, "ОбщийМетод", "() Экспорт");
    set_func(conn, fb, "ОбщийМетод", "() Экспорт");

    set_calls(
        conn,
        fc,
        &[
            ("ОбработкаПроведения", "МодульА.ОбщийМетод"), // → mod_a (квалификатор разводит коллизию)
            ("ОбработкаПроведения", "МодульБ.ОбщийМетод"), // → mod_b
            ("ОбработкаПроведения", "МодульА.НетТакого"),  // метода нет в А → NULL
            ("ОбработкаПроведения", "ЧужойМодуль.Метод"),  // квалификатор не общий модуль → NULL
        ],
    );

    build_call_graph(conn).unwrap();

    let key = |callee: &str| -> Option<String> {
        conn.query_row(
            "SELECT callee_proc_key FROM proc_call_graph \
             WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
            params![format!("{caller}::ОбработкаПроведения"), callee],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
    };

    // Коллизия имени разрешена квалификатором — точная привязка к нужному модулю.
    assert_eq!(key("МодульА.ОбщийМетод"), Some(format!("{mod_a}::ОбщийМетод")));
    assert_eq!(key("МодульБ.ОбщийМетод"), Some(format!("{mod_b}::ОбщийМетод")));
    // Метода нет в модуле, но квалификатор = реальный модуль → щадим, NULL.
    assert_eq!(key("МодульА.НетТакого"), None, "несуществующий метод модуля не привязывается");
    // Квалификатор — не общий модуль и не коллекция → трактуется как объектный
    // вызов и отсеивается пруном (строки больше нет).
    let exists_chuzhoy: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM proc_call_graph \
             WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
            params![format!("{caller}::ОбработкаПроведения"), "ЧужойМодуль.Метод"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exists_chuzhoy, 0, "вызов с неизвестным квалификатором отсеян пруном объектных вызовов");
}

#[test]
fn resolves_callee_keys_in_edt_layout() {
    // Выгрузка 1C:EDT кладёт модули без каталога Ext: общий модуль —
    // `CommonModules/<Имя>/Module.bsl`, менеджер — `<Тип>/<Объект>/ManagerModule.bsl`.
    // Раньше обе карты резолва строились по путям формата Конфигуратора и для
    // EDT оставались пустыми: квалифицированные вызовы не получали адреса, а
    // затем вырезались отсевом как «методы локального объекта» (E-8).
    let tmp = TempDir::new().unwrap();
    let st = fresh_storage(&tmp);
    let conn = st.conn();

    let caller = "src/Documents/Реализация/ObjectModule.bsl";
    let common = "src/CommonModules/МодульА/Module.bsl";
    let manager = "src/Catalogs/Контрагенты/ManagerModule.bsl";
    let fc = ensure_file(conn, caller);
    let fm = ensure_file(conn, common);
    let fg = ensure_file(conn, manager);

    set_func(conn, fc, "ОбработкаПроведения", "()");
    set_func(conn, fm, "ОбщийМетод", "() Экспорт");
    set_func(conn, fg, "МетодМенеджера", "() Экспорт");

    set_calls(
        conn,
        fc,
        &[
            ("ОбработкаПроведения", "МодульА.ОбщийМетод"),
            ("ОбработкаПроведения", "Справочники.Контрагенты.МетодМенеджера"),
        ],
    );

    build_call_graph(conn).unwrap();

    let key = |callee: &str| -> Option<Option<String>> {
        conn.query_row(
            "SELECT callee_proc_key FROM proc_call_graph                  WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
            params![format!("{caller}::ОбработкаПроведения"), callee],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
    };

    assert_eq!(
        key("МодульА.ОбщийМетод"),
        Some(Some(format!("{common}::ОбщийМетод"))),
        "вызов общего модуля в раскладке EDT обязан сохраниться и получить адрес"
    );
    assert_eq!(
        key("Справочники.Контрагенты.МетодМенеджера"),
        Some(Some(format!("{manager}::МетодМенеджера"))),
        "вызов менеджер-модуля в раскладке EDT обязан получить адрес"
    );
}

#[test]
fn prunes_glued_object_method_but_keeps_resolved_module_call() {
    // CORE B: у склеенных имён балласт отсеивается по методу-ПОСЛЕ-точки
    // (`Объект.Добавить` → `Добавить`); реальный вызов общего модуля,
    // резолвленный Tier C, при этом сохраняется (IS NULL guard).
    let tmp = TempDir::new().unwrap();
    let st = fresh_storage(&tmp);
    let conn = st.conn();

    let caller = "Documents/Реализация/Ext/ObjectModule.bsl";
    let cmod = "base/CommonModules/ОбщегоНазначения/Ext/Module.bsl";
    let fc = ensure_file(conn, caller);
    let fm = ensure_file(conn, cmod);

    set_func(conn, fc, "ОбработкаПроведения", "()");
    set_func(conn, fm, "РеальныйМетод", "() Экспорт");

    set_calls(
        conn,
        fc,
        &[
            ("ОбработкаПроведения", "Объект.Добавить"), // балласт по методу → удалить
            ("ОбработкаПроведения", "ОбщегоНазначения.РеальныйМетод"), // Tier C резолв → оставить
        ],
    );

    build_call_graph(conn).unwrap();

    let exists = |callee: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM proc_call_graph \
             WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
            params![format!("{caller}::ОбработкаПроведения"), callee],
            |r| r.get(0),
        )
        .unwrap()
    };

    assert_eq!(exists("Объект.Добавить"), 0, "склеенный балласт отсеивается по методу-после-точки");
    assert_eq!(
        exists("ОбщегоНазначения.РеальныйМетод"),
        1,
        "резолвленный вызов общего модуля сохраняется"
    );
}

#[test]
fn prunes_object_calls_protects_modules_collections_chains() {
    // CORE B прун по квалификатору: режем `Объект.Метод` (квалификатор —
    // переменная), но щадим общие модули, коллекции метаданных и цепочки.
    let tmp = TempDir::new().unwrap();
    let st = fresh_storage(&tmp);
    let conn = st.conn();

    let caller = "Documents/Реализация/Ext/ObjectModule.bsl";
    let cmod = "base/CommonModules/ОбщегоНазначения/Ext/Module.bsl";
    let fc = ensure_file(conn, caller);
    let fm = ensure_file(conn, cmod);
    set_func(conn, fc, "ОбработкаПроведения", "()");
    set_func(conn, fm, "РеальныйМетод", "() Экспорт");
    // Менеджер-модуль справочника с ЮЗЕР-экспортным методом.
    let mgr = "base/Catalogs/Контрагенты/Ext/ManagerModule.bsl";
    let fmgr = ensure_file(conn, mgr);
    set_func(conn, fmgr, "СоздатьПоНаименованию", "() Экспорт");

    set_calls(
        conn,
        fc,
        &[
            ("ОбработкаПроведения", "Объект.ПроизвольныйМетод"), // объект (1 точка) → удалить
            ("ОбработкаПроведения", "Запрос.ВыполнитьПакет"),   // объект (1 точка) → удалить
            ("ОбработкаПроведения", "Запрос.Поле.Значение"),    // объектная цепочка (2 точки) → удалить
            ("ОбработкаПроведения", "ОбщегоНазначения.РеальныйМетод"), // модуль (Tier C) → оставить
            ("ОбработкаПроведения", "ОбщегоНазначения.НетТакого"),     // модуль, метод не экспортен → NULL, щадим
            ("ОбработкаПроведения", "Справочники.НайтиПоНаименованию"), // коллекция (1 точка) → щадим
            ("ОбработкаПроведения", "Справочники.Контрагенты.СоздатьПоНаименованию"), // менеджер (Tier D) → резолв
            ("ОбработкаПроведения", "Справочники.Контрагенты.ПустаяСсылка"), // платформенный метод менеджера → удалить
        ],
    );

    build_call_graph(conn).unwrap();

    let exists = |callee: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM proc_call_graph \
             WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
            params![format!("{caller}::ОбработкаПроведения"), callee],
            |r| r.get(0),
        )
        .unwrap()
    };
    let key = |callee: &str| -> Option<String> {
        conn.query_row(
            "SELECT callee_proc_key FROM proc_call_graph \
             WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
            params![format!("{caller}::ОбработкаПроведения"), callee],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
    };

    assert_eq!(exists("Объект.ПроизвольныйМетод"), 0, "объектный вызов (1 точка) отсеян");
    assert_eq!(exists("Запрос.ВыполнитьПакет"), 0, "объектный вызов (1 точка) отсеян");
    assert_eq!(exists("Запрос.Поле.Значение"), 0, "объектная цепочка (2 точки) отсеяна");
    assert_eq!(exists("ОбщегоНазначения.РеальныйМетод"), 1, "общий модуль (резолв) сохранён");
    assert_eq!(exists("ОбщегоНазначения.НетТакого"), 1, "имя общего модуля щадим даже при NULL");
    assert_eq!(exists("Справочники.НайтиПоНаименованию"), 1, "коллекция (1 точка) сохранена");
    assert_eq!(
        key("Справочники.Контрагенты.СоздатьПоНаименованию"),
        Some(format!("{mgr}::СоздатьПоНаименованию")),
        "менеджер-вызов резолвлен в ManagerModule (Tier D)"
    );
    assert_eq!(exists("Справочники.Контрагенты.ПустаяСсылка"), 0, "платформенный метод менеджера отсеян");
}

#[test]
fn incremental_object_xml_matches_full() {
    let cfg = r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Document>Реализация</Document>
</ChildObjects></Configuration></MetaDataObject>"#;
    let doc_v1 = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Document uuid="root"><Properties><Name>Реализация</Name></Properties>
<ChildObjects>
  <Attribute uuid="a1"><Properties><Name>Контрагент</Name>
    <Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
  </Properties></Attribute>
</ChildObjects>
  </Document>
</MetaDataObject>"#;
    // v2: реквизит переименован + сменил тип ссылки + добавлен второй.
    let doc_v2 = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Document uuid="root"><Properties><Name>Реализация</Name></Properties>
<ChildObjects>
  <Attribute uuid="a1"><Properties><Name>Партнёр</Name>
    <Type><v8:Type>cfg:CatalogRef.Организации</v8:Type></Type>
  </Properties></Attribute>
  <Attribute uuid="a2"><Properties><Name>Склад</Name>
    <Type><v8:Type>cfg:CatalogRef.Склады</v8:Type></Type>
  </Properties></Attribute>
</ChildObjects>
  </Document>
</MetaDataObject>"#;

    // truth: репо сразу в версии v2, полный пересбор.
    let tmp_t = TempDir::new().unwrap();
    let repo_t = tmp_t.path().join("repo");
    write(&repo_t.join("Configuration.xml"), cfg);
    write(&repo_t.join("Documents").join("Реализация.xml"), doc_v2);
    let mut st_t = fresh_storage(&tmp_t);
    run_index_extras(&repo_t, &mut st_t).unwrap();

    // incr: репо v1 → полный пересбор → правка XML на v2 → инкремент.
    let tmp_i = TempDir::new().unwrap();
    let repo_i = tmp_i.path().join("repo");
    write(&repo_i.join("Configuration.xml"), cfg);
    let doc_path = repo_i.join("Documents").join("Реализация.xml");
    write(&doc_path, doc_v1);
    let mut st_i = fresh_storage(&tmp_i);
    run_index_extras(&repo_i, &mut st_i).unwrap();
    write(&doc_path, doc_v2);
    run_incremental_extras(&repo_i, &mut st_i, &[doc_path.clone()], &[]).unwrap();

    assert_eq!(
        snapshot_dl(st_i.conn()),
        snapshot_dl(st_t.conn()),
        "data_links после инкремента != полному пересбору"
    );
    assert_eq!(
        snapshot_attrs(st_i.conn()),
        snapshot_attrs(st_t.conn()),
        "attributes_json после инкремента != полному пересбору"
    );
}

// ── Уход заимствователя / изменение копии расширения: data_links должны
//    строиться слиянием по копиям (симметрично bulk index_data_links) ──────
//
// Общий фикстур: база Catalog.Контрагенты с реквизитом-ссылкой ОсновнойГород
// → Catalog.Города; EF_A заимствует Контрагенты (Adopted) с ДОБАВЛЕННЫМ своим
// реквизитом ДопРегион → Catalog.Регионы. Полный индекс даёт ДВА ребра:
// Контрагенты→Города (база) и Контрагенты→Регионы (расширение).
fn write_borrow_repo(repo: &Path) {
    write(
        &repo.join("base").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("Catalogs").join("Контрагенты.xml"),
        r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root"><Properties><Name>Контрагенты</Name></Properties>
<ChildObjects>
  <Attribute uuid="a1"><Properties><Name>ОсновнойГород</Name>
    <Type><v8:Type>cfg:CatalogRef.Города</v8:Type></Type>
  </Properties></Attribute>
</ChildObjects>
  </Catalog>
</MetaDataObject>"#,
    );
    write(
        &repo.join("base").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Контрагенты" id="k1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
    write(
        &repo.join("extensions").join("EF_A").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        &repo.join("extensions").join("EF_A").join("Catalogs").join("Контрагенты.xml"),
        r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root"><Properties><Name>Контрагенты</Name><ObjectBelonging>Adopted</ObjectBelonging></Properties>
<ChildObjects>
  <Attribute uuid="b1"><Properties><Name>ДопРегион</Name>
    <Type><v8:Type>cfg:CatalogRef.Регионы</v8:Type></Type>
  </Properties></Attribute>
</ChildObjects>
  </Catalog>
</MetaDataObject>"#,
    );
    write(
        &repo.join("extensions").join("EF_A").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Контрагенты" id="k1" configVersion="v1"/></ConfigVersions></ConfigDumpInfo>"#,
    );
}

// Снять заимствование EF_A: удалить его копию объекта с диска, опись — пустая
// (расширение выжило, но заимствует пусто). Возвращает (путь_описи, путь_копии).
fn drop_ef_a_borrow(repo: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let copy = repo.join("extensions").join("EF_A").join("Catalogs").join("Контрагенты.xml");
    let _ = std::fs::remove_file(&copy);
    let dump = repo.join("extensions").join("EF_A").join("ConfigDumpInfo.xml");
    write(&dump, r#"<ConfigDumpInfo><ConfigVersions></ConfigVersions></ConfigDumpInfo>"#);
    (dump, copy)
}

#[test]
fn incremental_borrower_drop_keeps_data_links() {
    // Уход заимствователя, delete-событие копии ДОСТАВЛЕНО. Дефект: пофайловый
    // update_data_links_for_object по удалённой копии сносит ВСЕ рёбра объекта
    // (в т.ч. базовое) и не переразбирает (файла нет) → data_links пуст.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    write_borrow_repo(&repo);
    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();

    let (dump, copy) = drop_ef_a_borrow(&repo);
    run_incremental_extras(&repo, &mut storage, &[dump], &[copy]).unwrap();

    // Эталон: финальное состояние (только база) с нуля.
    let tmp_t = TempDir::new().unwrap();
    let repo_t = tmp_t.path().join("repo");
    write_borrow_repo(&repo_t);
    drop_ef_a_borrow(&repo_t);
    let mut st_t = fresh_storage(&tmp_t);
    run_index_extras(&repo_t, &mut st_t).unwrap();

    assert_eq!(
        snapshot_dl(storage.conn()),
        snapshot_dl(st_t.conn()),
        "data_links после ухода заимствователя (delete доставлен) != полному пересбору"
    );
    assert_eq!(
        snapshot_attrs(storage.conn()),
        snapshot_attrs(st_t.conn()),
        "attributes_json != полному пересбору"
    );
}

#[test]
fn incremental_borrower_drop_opis_only_keeps_data_links() {
    // Уход заимствователя, delete-событие копии ПОТЕРЯНО watcher-ом: в пачке
    // только MODIFY описи расширения. Дефект: remerge_object (по опись-событию)
    // сейчас data_links не трогает → EF_A-ребро остаётся фантомом.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    write_borrow_repo(&repo);
    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();

    let (dump, _copy) = drop_ef_a_borrow(&repo); // копия удалена с диска, но события delete нет
    run_incremental_extras(&repo, &mut storage, &[dump], &[]).unwrap();

    let tmp_t = TempDir::new().unwrap();
    let repo_t = tmp_t.path().join("repo");
    write_borrow_repo(&repo_t);
    drop_ef_a_borrow(&repo_t);
    let mut st_t = fresh_storage(&tmp_t);
    run_index_extras(&repo_t, &mut st_t).unwrap();

    assert_eq!(
        snapshot_dl(storage.conn()),
        snapshot_dl(st_t.conn()),
        "data_links после ухода заимствователя (delete потерян) != полному пересбору"
    );
}

#[test]
fn incremental_borrower_drop_rebuilds_metadata_modules() {
    // Уход заимствователя: EF_A-копия объекта удалена + опись EF_A пуста, но
    // .bsl-модуль копии остаётся на диске сиротой. Дефект (до фикса):
    // remerge_object не трогал metadata_modules → строка модуля EF_A
    // (со своим config_version) остаётся, расходясь с полным reindex, где
    // модуль-сирота отсеивается (owner XML удалён). Фикс: remerge пере-собирает
    // модули объекта (DELETE + обход roots). Пойман федеративным smoke на типовой торговой конфигурации.
    fn write_repo(repo: &Path) {
        write(
            &repo.join("base").join("Configuration.xml"),
            r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
        );
        write(
            &repo.join("base").join("Catalogs").join("Контрагенты.xml"),
            r#"<MetaDataObject><Catalog uuid="cbase"><Properties><Name>Контрагенты</Name></Properties></Catalog></MetaDataObject>"#,
        );
        write(
            &repo.join("base").join("Catalogs").join("Контрагенты").join("Ext").join("ManagerModule.bsl"),
            "Процедура П() Экспорт\nКонецПроцедуры",
        );
        write(
            &repo.join("base").join("ConfigDumpInfo.xml"),
            r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Контрагенты" id="cbase" configVersion="VER-base"/></ConfigVersions></ConfigDumpInfo>"#,
        );
        write(
            &repo.join("extensions").join("EF_A").join("Configuration.xml"),
            r#"<MetaDataObject><Configuration><ChildObjects><Catalog>Контрагенты</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
        );
        write(
            &repo.join("extensions").join("EF_A").join("Catalogs").join("Контрагенты.xml"),
            r#"<MetaDataObject><Catalog uuid="cbase"><Properties><Name>Контрагенты</Name><ObjectBelonging>Adopted</ObjectBelonging></Properties></Catalog></MetaDataObject>"#,
        );
        write(
            &repo.join("extensions").join("EF_A").join("Catalogs").join("Контрагенты").join("Ext").join("ManagerModule.bsl"),
            "Процедура П() Экспорт\nКонецПроцедуры",
        );
        write(
            &repo.join("extensions").join("EF_A").join("ConfigDumpInfo.xml"),
            r#"<ConfigDumpInfo><ConfigVersions><Metadata name="Catalog.Контрагенты" id="cbase" configVersion="VER-ext"/></ConfigVersions></ConfigDumpInfo>"#,
        );
    }
    let snap_mods = |st: &Storage| -> Vec<(String, String, Option<String>, String)> {
        st.conn()
            .prepare(
                "SELECT full_name, object_name, config_version, extension_name \
                 FROM metadata_modules WHERE repo=? ORDER BY full_name, extension_name",
            )
            .unwrap()
            .query_map(params![REPO_DEFAULT], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    write_repo(&repo);
    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();

    // уход: удалить EF_A-копию объекта + опустошить опись EF_A; .bsl-модуль остаётся
    let copy = repo.join("extensions").join("EF_A").join("Catalogs").join("Контрагенты.xml");
    std::fs::remove_file(&copy).unwrap();
    let dump = repo.join("extensions").join("EF_A").join("ConfigDumpInfo.xml");
    write(&dump, r#"<ConfigDumpInfo><ConfigVersions></ConfigVersions></ConfigDumpInfo>"#);
    run_incremental_extras(&repo, &mut storage, &[dump], &[copy]).unwrap();

    // эталон: полный пересбор финального состояния (только база)
    let tmp_t = TempDir::new().unwrap();
    let repo_t = tmp_t.path().join("repo");
    write_repo(&repo_t);
    std::fs::remove_file(
        repo_t.join("extensions").join("EF_A").join("Catalogs").join("Контрагенты.xml"),
    )
    .unwrap();
    write(
        &repo_t.join("extensions").join("EF_A").join("ConfigDumpInfo.xml"),
        r#"<ConfigDumpInfo><ConfigVersions></ConfigVersions></ConfigDumpInfo>"#,
    );
    let mut st_t = fresh_storage(&tmp_t);
    run_index_extras(&repo_t, &mut st_t).unwrap();

    assert_eq!(
        snap_mods(&storage),
        snap_mods(&st_t),
        "metadata_modules после ухода заимствователя != полному пересбору"
    );
}

#[test]
fn incremental_ext_copy_change_keeps_base_data_links() {
    // Изменена ТОЛЬКО копия расширения (объект жив в базе и в EF_A). Дефект:
    // update_data_links_for_object по копии EF_A сносит все рёбра и разбирает
    // только EF_A → базовое ребро Контрагенты→Города теряется.
    let modified_ef_copy = r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root"><Properties><Name>Контрагенты</Name><ObjectBelonging>Adopted</ObjectBelonging></Properties>
<ChildObjects>
  <Attribute uuid="b1"><Properties><Name>ДопОкруг</Name>
    <Type><v8:Type>cfg:CatalogRef.Округа</v8:Type></Type>
  </Properties></Attribute>
</ChildObjects>
  </Catalog>
</MetaDataObject>"#;

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    write_borrow_repo(&repo);
    let mut storage = fresh_storage(&tmp);
    run_index_extras(&repo, &mut storage).unwrap();

    let ef_copy = repo.join("extensions").join("EF_A").join("Catalogs").join("Контрагенты.xml");
    write(&ef_copy, modified_ef_copy);
    run_incremental_extras(&repo, &mut storage, &[ef_copy], &[]).unwrap();

    let tmp_t = TempDir::new().unwrap();
    let repo_t = tmp_t.path().join("repo");
    write_borrow_repo(&repo_t);
    write(
        &repo_t.join("extensions").join("EF_A").join("Catalogs").join("Контрагенты.xml"),
        modified_ef_copy,
    );
    let mut st_t = fresh_storage(&tmp_t);
    run_index_extras(&repo_t, &mut st_t).unwrap();

    assert_eq!(
        snapshot_dl(storage.conn()),
        snapshot_dl(st_t.conn()),
        "data_links после изменения копии расширения != полному пересбору (базовое ребро потеряно?)"
    );
    assert_eq!(
        snapshot_attrs(storage.conn()),
        snapshot_attrs(st_t.conn()),
        "attributes_json != полному пересбору"
    );
}

#[test]
fn incremental_massive_object_change_matches_full() {
    // Масштаб: N объектов кольцом ссылок в базе, все заимствованы EF_A с
    // добавленным реквизитом. Массивная пачка: все базовые файлы меняют реф
    // (кольцо 1→2) + сняты ВСЕ заимствования (удалены копии EF_A). Единый путь
    // должен построчно сойтись к полному пересбору финального состояния.
    const N: usize = 60;

    fn base_obj_xml(i: usize, to: usize) -> String {
        format!(
            "<MetaDataObject xmlns:v8=\"http://v8.1c.ru/8.3/data/core\">\n\
             <Catalog uuid=\"r{i}\"><Properties><Name>Спр{i}</Name></Properties>\n\
             <ChildObjects><Attribute uuid=\"a{i}\"><Properties><Name>Реф</Name>\n\
             <Type><v8:Type>cfg:CatalogRef.Спр{to}</v8:Type></Type>\n\
             </Properties></Attribute></ChildObjects></Catalog></MetaDataObject>"
        )
    }

    // Собрать репо: base с реф-сдвигом ref_shift; borrow=true → EF_A заимствует
    // все объекты (Adopted) с реквизитом-ссылкой на Спр{(i+2)%N}.
    fn build(repo: &Path, borrow: bool, ref_shift: usize) {
        let mut base_children = String::new();
        let mut base_dump = String::from("<ConfigDumpInfo><ConfigVersions>");
        for i in 0..N {
            base_children.push_str(&format!("<Catalog>Спр{i}</Catalog>"));
            base_dump.push_str(&format!(
                "<Metadata name=\"Catalog.Спр{i}\" id=\"c{i}\" configVersion=\"v1\"/>"
            ));
            write(
                &repo.join("base").join("Catalogs").join(format!("Спр{i}.xml")),
                &base_obj_xml(i, (i + ref_shift) % N),
            );
        }
        base_dump.push_str("</ConfigVersions></ConfigDumpInfo>");
        write(
            &repo.join("base").join("Configuration.xml"),
            &format!("<MetaDataObject><Configuration><ChildObjects>{base_children}</ChildObjects></Configuration></MetaDataObject>"),
        );
        write(&repo.join("base").join("ConfigDumpInfo.xml"), &base_dump);

        if borrow {
            let mut ef_children = String::new();
            let mut ef_dump = String::from("<ConfigDumpInfo><ConfigVersions>");
            for i in 0..N {
                ef_children.push_str(&format!("<Catalog>Спр{i}</Catalog>"));
                ef_dump.push_str(&format!(
                    "<Metadata name=\"Catalog.Спр{i}\" id=\"c{i}\" configVersion=\"v1\"/>"
                ));
                let to = (i + 2) % N;
                write(
                    &repo.join("extensions").join("EF_A").join("Catalogs").join(format!("Спр{i}.xml")),
                    &format!(
                        "<MetaDataObject xmlns:v8=\"http://v8.1c.ru/8.3/data/core\">\n\
                         <Catalog uuid=\"r{i}\"><Properties><Name>Спр{i}</Name><ObjectBelonging>Adopted</ObjectBelonging></Properties>\n\
                         <ChildObjects><Attribute uuid=\"e{i}\"><Properties><Name>ЭкстРеф</Name>\n\
                         <Type><v8:Type>cfg:CatalogRef.Спр{to}</v8:Type></Type>\n\
                         </Properties></Attribute></ChildObjects></Catalog></MetaDataObject>"
                    ),
                );
            }
            ef_dump.push_str("</ConfigVersions></ConfigDumpInfo>");
            write(
                &repo.join("extensions").join("EF_A").join("Configuration.xml"),
                &format!("<MetaDataObject><Configuration><ChildObjects>{ef_children}</ChildObjects></Configuration></MetaDataObject>"),
            );
            write(&repo.join("extensions").join("EF_A").join("ConfigDumpInfo.xml"), &ef_dump);
        } else {
            write(
                &repo.join("extensions").join("EF_A").join("Configuration.xml"),
                r#"<MetaDataObject><Configuration><ChildObjects></ChildObjects></Configuration></MetaDataObject>"#,
            );
            write(
                &repo.join("extensions").join("EF_A").join("ConfigDumpInfo.xml"),
                r#"<ConfigDumpInfo><ConfigVersions></ConfigVersions></ConfigDumpInfo>"#,
            );
        }
    }

    // Эталон: финальное состояние (реф-сдвиг 2, заимствование снято).
    let tmp_t = TempDir::new().unwrap();
    let repo_t = tmp_t.path().join("repo");
    build(&repo_t, false, 2);
    let mut st_t = fresh_storage(&tmp_t);
    run_index_extras(&repo_t, &mut st_t).unwrap();

    // Инкремент: исходное (сдвиг 1, всё заимствовано) → полный индекс → массивная пачка.
    let tmp_i = TempDir::new().unwrap();
    let repo_i = tmp_i.path().join("repo");
    build(&repo_i, true, 1);
    let mut st_i = fresh_storage(&tmp_i);
    run_index_extras(&repo_i, &mut st_i).unwrap();

    let mut changed: Vec<std::path::PathBuf> = Vec::new();
    let mut deleted: Vec<std::path::PathBuf> = Vec::new();
    for i in 0..N {
        let p = repo_i.join("base").join("Catalogs").join(format!("Спр{i}.xml"));
        write(&p, &base_obj_xml(i, (i + 2) % N)); // база: реф-сдвиг 1→2
        changed.push(p);
        let c = repo_i.join("extensions").join("EF_A").join("Catalogs").join(format!("Спр{i}.xml"));
        std::fs::remove_file(&c).unwrap(); // снять заимствование: удалить копию
        deleted.push(c);
    }
    write(
        &repo_i.join("extensions").join("EF_A").join("Configuration.xml"),
        r#"<MetaDataObject><Configuration><ChildObjects></ChildObjects></Configuration></MetaDataObject>"#,
    );
    let ef_dump = repo_i.join("extensions").join("EF_A").join("ConfigDumpInfo.xml");
    write(&ef_dump, r#"<ConfigDumpInfo><ConfigVersions></ConfigVersions></ConfigDumpInfo>"#);
    changed.push(ef_dump);

    run_incremental_extras(&repo_i, &mut st_i, &changed, &deleted).unwrap();

    assert_eq!(
        snapshot_dl(st_i.conn()),
        snapshot_dl(st_t.conn()),
        "data_links после массивной пачки != полному пересбору"
    );
    assert_eq!(
        snapshot_attrs(st_i.conn()),
        snapshot_attrs(st_t.conn()),
        "attributes_json после массивной пачки != полному пересбору"
    );
}

#[test]
fn incremental_call_graph_direct_matches_full() {
    // Репо с подпиской и формой — проверяем, что инкремент .bsl
    // пересобирает только слой direct и НЕ затирает subscription/form_event.
    let cfg = r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Document>Реализация</Document>
</ChildObjects></Configuration></MetaDataObject>"#;
    let sub = r#"<?xml version="1.0"?>
<MetaDataObject>
  <EventSubscription><Properties>
<Name>Подписка1</Name>
<Source><Type><v8:Type>cfg:DocumentRef.Реализация</v8:Type></Type></Source>
<Event>ПриЗаписи</Event>
<Handler>ОбщийМодуль.Обработчик</Handler>
  </Properties></EventSubscription>
</MetaDataObject>"#;
    let form = r#"<?xml version="1.0"?>
<Form><Events>
  <Event name="ПриОткрытии">ПриОткрытииСервер</Event>
</Events></Form>"#;

    let build = |tmp: &TempDir| -> (std::path::PathBuf, Storage) {
        let repo = tmp.path().join("repo");
        write(&repo.join("Configuration.xml"), cfg);
        write(&repo.join("EventSubscriptions").join("Подписка1.xml"), sub);
        write(
            &repo
                .join("Documents")
                .join("Реализация")
                .join("Forms")
                .join("ФормаДокумента")
                .join("Ext")
                .join("Form.xml"),
            form,
        );
        (repo, fresh_storage(tmp))
    };

    // truth: calls = v2, полный пересбор.
    let tmp_t = TempDir::new().unwrap();
    let (repo_t, mut st_t) = build(&tmp_t);
    let fid_t = ensure_file(st_t.conn(), "Documents/Реализация/Ext/ObjectModule.bsl");
    set_calls(st_t.conn(), fid_t, &[("ПриЗаписи", "ВыполнитьC"), ("ПриЗаписи", "Общее")]);
    run_index_extras(&repo_t, &mut st_t).unwrap();

    // incr: calls = v1 → полный пересбор → правка .bsl (calls → v2) → инкремент.
    let tmp_i = TempDir::new().unwrap();
    let (repo_i, mut st_i) = build(&tmp_i);
    let fid_i = ensure_file(st_i.conn(), "Documents/Реализация/Ext/ObjectModule.bsl");
    set_calls(st_i.conn(), fid_i, &[("ПриЗаписи", "ВыполнитьB"), ("ПриЗаписи", "Общее")]);
    run_index_extras(&repo_i, &mut st_i).unwrap();
    set_calls(st_i.conn(), fid_i, &[("ПриЗаписи", "ВыполнитьC"), ("ПриЗаписи", "Общее")]);
    let bsl_path = repo_i
        .join("Documents")
        .join("Реализация")
        .join("Ext")
        .join("ObjectModule.bsl");
    run_incremental_extras(&repo_i, &mut st_i, &[bsl_path], &[]).unwrap();

    assert_eq!(
        snapshot_pcg(st_i.conn()),
        snapshot_pcg(st_t.conn()),
        "proc_call_graph после инкремента .bsl != полному пересбору"
    );
}

#[test]
fn incremental_call_graph_multifile_batch_matches_full() {
    // Батч из ДВУХ .bsl за один run_incremental_extras: два общих модуля
    // кросс-ссылаются экспортными методами (`МодульА.ПроцА` ↔ `МодульБ.ПроцБ`).
    // После рефакторинга резолв callee_proc_key идёт ОДИН раз на батч (а не
    // пофайлово) — проверяем, что итоговый proc_call_graph совпадает с полным
    // пересбором, и что обе кросс-ссылки резолвнуты в адреса общих модулей.
    let cfg = r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects></ChildObjects></Configuration></MetaDataObject>"#;
    let pa = "CommonModules/МодульА/Ext/Module.bsl";
    let pb = "CommonModules/МодульБ/Ext/Module.bsl";

    let seed = |st: &Storage, a: &[(&str, &str)], b: &[(&str, &str)]| {
        let conn = st.conn();
        let fa = ensure_file(conn, pa);
        let fb = ensure_file(conn, pb);
        set_func(conn, fa, "ПроцА", "() Экспорт");
        set_func(conn, fb, "ПроцБ", "() Экспорт");
        set_calls(conn, fa, a);
        set_calls(conn, fb, b);
    };

    // truth: конечные вызовы, полный пересбор.
    let tmp_t = TempDir::new().unwrap();
    let repo_t = tmp_t.path().join("repo");
    write(&repo_t.join("Configuration.xml"), cfg);
    let mut st_t = fresh_storage(&tmp_t);
    seed(&st_t, &[("ПроцА", "МодульБ.ПроцБ")], &[("ПроцБ", "МодульА.ПроцА")]);
    run_index_extras(&repo_t, &mut st_t).unwrap();

    // incr: v1-вызовы → полный пересбор → правка ОБОИХ модулей (v2) → батч-инкремент.
    let tmp_i = TempDir::new().unwrap();
    let repo_i = tmp_i.path().join("repo");
    write(&repo_i.join("Configuration.xml"), cfg);
    let mut st_i = fresh_storage(&tmp_i);
    seed(&st_i, &[("ПроцА", "СтароеИмя")], &[("ПроцБ", "ЕщёСтарое")]);
    run_index_extras(&repo_i, &mut st_i).unwrap();
    // v2: те же вызовы, что в truth.
    {
        let conn = st_i.conn();
        let fa = ensure_file(conn, pa);
        let fb = ensure_file(conn, pb);
        set_calls(conn, fa, &[("ПроцА", "МодульБ.ПроцБ")]);
        set_calls(conn, fb, &[("ПроцБ", "МодульА.ПроцА")]);
    }
    let bsl_a = repo_i.join("CommonModules").join("МодульА").join("Ext").join("Module.bsl");
    let bsl_b = repo_i.join("CommonModules").join("МодульБ").join("Ext").join("Module.bsl");
    run_incremental_extras(&repo_i, &mut st_i, &[bsl_a, bsl_b], &[]).unwrap();

    assert_eq!(
        snapshot_pcg(st_i.conn()),
        snapshot_pcg(st_t.conn()),
        "proc_call_graph после батч-инкремента 2 файлов != полному пересбору"
    );

    // Явная проверка: обе кросс-ссылки резолвнуты в адреса общих модулей.
    let key = |callee: &str| -> Option<String> {
        st_i.conn()
            .query_row(
                "SELECT callee_proc_key FROM proc_call_graph                      WHERE repo=?1 AND call_type='direct' AND callee_proc_name=?2",
                params![REPO_DEFAULT, callee],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
    };
    assert_eq!(key("МодульБ.ПроцБ"), Some(format!("{pb}::ПроцБ")), "А→Б резолвнут");
    assert_eq!(key("МодульА.ПроцА"), Some(format!("{pa}::ПроцА")), "Б→А резолвнут");
}

#[test]
fn incremental_direct_shared_edge_survives() {
    // Ключевое свойство per-file при path-привязке ключей: F1 и F2 дают
    // РАЗНЫЕ рёбра — `F1.bsl::A->B` и `F2.bsl::A->B` (caller_proc_key несёт
    // путь файла). F1 дополнительно даёт `F1.bsl::A->C`. Правим F1 → у него
    // остаётся только A->B. Ожидаем: ребро F2 (`F2.bsl::A->B`) не зависит от
    // правки F1 и выживает; `F1.bsl::A->B` остаётся; `F1.bsl::A->C` исчезает.
    // Результат обязан совпасть с полным пересбором.
    fn setup(tmp: &TempDir, f1_edges: &[(&str, &str)]) -> (std::path::PathBuf, Storage, i64) {
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let st = fresh_storage(tmp);
        let f1 = ensure_file(st.conn(), "F1.bsl");
        let f2 = ensure_file(st.conn(), "F2.bsl");
        set_calls(st.conn(), f1, f1_edges);
        set_calls(st.conn(), f2, &[("A", "B")]);
        (repo, st, f1)
    }

    // truth: конечное состояние сразу (F1={A->B}, F2={A->B}), полный пересбор.
    let tmp_t = TempDir::new().unwrap();
    let (repo_t, mut st_t, _) = setup(&tmp_t, &[("A", "B")]);
    run_index_extras(&repo_t, &mut st_t).unwrap();

    // incr: F1 сперва {A->B, A->C}; полный пересбор; затем F1 -> {A->B}; инкремент F1.
    let tmp_i = TempDir::new().unwrap();
    let (repo_i, mut st_i, f1_i) = setup(&tmp_i, &[("A", "B"), ("A", "C")]);
    run_index_extras(&repo_i, &mut st_i).unwrap();
    set_calls(st_i.conn(), f1_i, &[("A", "B")]);
    run_incremental_extras(&repo_i, &mut st_i, &[repo_i.join("F1.bsl")], &[]).unwrap();

    let s_i = snapshot_pcg(st_i.conn());
    assert_eq!(
        s_i,
        snapshot_pcg(st_t.conn()),
        "after incremental != full rebuild (shared edge)"
    );
    assert!(
        s_i.iter().any(|(c, e, _)| c == "F2.bsl::A" && e == "B"),
        "ребро F2 (F2.bsl::A->B) не зависит от правки F1 и выживает"
    );
    assert!(
        s_i.iter().any(|(c, e, _)| c == "F1.bsl::A" && e == "B"),
        "F1.bsl::A->B остаётся (F1 его по-прежнему даёт)"
    );
    assert!(
        !s_i.iter().any(|(_, e, _)| e == "C"),
        "F1.bsl::A->C должно исчезнуть (F1 его больше не даёт)"
    );
}

#[test]
fn backfill_keys_fill_lowercase_cyrillic() {
    use rusqlite::Connection;
    let conn = Connection::open_in_memory().unwrap();
    for ddl in crate::schema::SCHEMA_EXTENSIONS {
        conn.execute_batch(ddl).unwrap();
    }
    // Ребро без ключа (как сразу после INSERT) → backfill заполняет lower().
    conn.execute(
        "INSERT INTO data_links (repo, from_object, from_path, to_object, link_kind) \
         VALUES ('default','A','p','Document.ЗаказКлиента','attr')",
        [],
    )
    .unwrap();
    backfill_data_link_keys(&conn).unwrap();
    let key: String = conn
        .query_row(
            "SELECT to_object_key FROM data_links WHERE to_object='Document.ЗаказКлиента'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(key, "document.заказклиента");

    conn.execute(
        "INSERT INTO role_rights (repo, role_name, object_name, right_name) \
         VALUES ('default','Менеджер','Document.ЗаказКлиента','Read')",
        [],
    )
    .unwrap();
    backfill_role_right_keys(&conn).unwrap();
    let rk: String = conn
        .query_row(
            "SELECT object_name_key FROM role_rights WHERE right_name='Read'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rk, "document.заказклиента");
}

#[test]
fn edt_layer_indexes_nested_subsystems() {
    // Вложенные подсистемы EDT лежат деревом
    // `Subsystems/<Родитель>/Subsystems/<Ребёнок>/<Ребёнок>.mdo`, а обход шёл
    // ровно на два уровня — в реестр попадали только верхнеуровневые (E-2).
    // Вложенность есть только у подсистем: у остальных типов объекты лежат
    // строго на втором уровне, поэтому проверяем обе раскладки сразу.
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");

    let mdo = |name: &str, meta: &str| {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <mdclass:{meta} xmlns:mdclass=\"http://g5.1c.ru/v8/dt/metadata/mdclass\">\n\
             <name>{name}</name>\n\
             <synonym><key>ru</key><value>Синоним {name}</value></synonym>\n\
             </mdclass:{meta}>"
        )
    };

    let subs = src.join("Subsystems");
    write(
        &subs.join("Продажи").join("Продажи.mdo"),
        &mdo("Продажи", "Subsystem"),
    );
    write(
        &subs
            .join("Продажи")
            .join("Subsystems")
            .join("Розница")
            .join("Розница.mdo"),
        &mdo("Розница", "Subsystem"),
    );
    write(
        &subs
            .join("Продажи")
            .join("Subsystems")
            .join("Розница")
            .join("Subsystems")
            .join("Касса")
            .join("Касса.mdo"),
        &mdo("Касса", "Subsystem"),
    );
    write(
        &src.join("Catalogs").join("Товары").join("Товары.mdo"),
        &mdo("Товары", "Catalog"),
    );

    let st = fresh_storage(&tmp);
    run_edt_metadata_layer(&src, st.conn()).unwrap();

    let synonym_of = |full_name: &str| -> Option<String> {
        st.conn()
            .query_row(
                "SELECT synonym FROM metadata_objects WHERE full_name = ?1",
                params![full_name],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
    };

    assert_eq!(
        synonym_of("Subsystem.Продажи").as_deref(),
        Some("Синоним Продажи"),
        "верхнеуровневая подсистема как и раньше в реестре"
    );
    assert_eq!(
        synonym_of("Subsystem.Розница").as_deref(),
        Some("Синоним Розница"),
        "вложенная подсистема первого уровня обязана попасть в реестр"
    );
    assert_eq!(
        synonym_of("Subsystem.Касса").as_deref(),
        Some("Синоним Касса"),
        "вложенная подсистема второго уровня обязана попасть в реестр"
    );
    assert_eq!(
        synonym_of("Catalog.Товары").as_deref(),
        Some("Синоним Товары"),
        "обычный тип объектов обходом не задет"
    );

    let total: i64 = st
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM metadata_objects WHERE repo = ?",
            params![REPO_DEFAULT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 4, "три подсистемы дерева и один справочник");
}

#[test]
fn edt_layer_indexes_role_rights() {
    // Права роли в EDT лежат отдельным файлом `Roles/<Имя>/Rights.rights`
    // (у Конфигуратора — `Roles/<Имя>/Ext/Rights.xml`), и фаза для EDT не
    // выполнялась вовсе: таблица оставалась пустой при 649 ролях (E-1).
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    write(
        &src.join("Roles").join("Бухгалтер").join("Rights.rights"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="Rights">
    <object>
        <name>Catalog.Контрагенты</name>
        <right><name>Read</name><value>true</value></right>
        <right><name>Update</name><value>false</value></right>
    </object>
    <object>
        <name>Document.ЗаказКлиента</name>
        <right><name>Posting</name><value>true</value></right>
    </object>
</Rights>"#,
    );
    // Каталог роли без файла прав — не должен ронять проход.
    std::fs::create_dir_all(src.join("Roles").join("ПустаяРоль")).unwrap();

    let st = fresh_storage(&tmp);
    run_edt_role_rights(&src, st.conn()).unwrap();

    let total: i64 = st
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM role_rights WHERE repo = ?",
            params![REPO_DEFAULT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 2, "берутся только выданные права, отказ не хранится");

    let role: String = st
        .conn()
        .query_row(
            "SELECT role_name FROM role_rights WHERE object_name='Document.ЗаказКлиента'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(role, "Бухгалтер", "имя роли — каталог-родитель файла прав");

    let key: Option<String> = st
        .conn()
        .query_row(
            "SELECT object_name_key FROM role_rights WHERE object_name='Catalog.Контрагенты'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        key.as_deref(),
        Some("catalog.контрагенты"),
        "ключ поиска достраивается так же, как в формате Конфигуратора"
    );
}

#[test]
fn edt_layer_indexes_modules() {
    // В EDT у объекта нет каталога Ext, модуль формы лежит в
    // `Forms/<Ф>/Module.bsl`, а идентификаторы форм и команд записаны внутри
    // `.mdo` владельца. Фаза перечня модулей для EDT не выполнялась вовсе —
    // таблица оставалась пустой при 18 267 файлах `.bsl` (E-1).
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");

    write(
        &src.join("Catalogs").join("Товары").join("Товары.mdo"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<mdclass:Catalog xmlns:mdclass="http://g5.1c.ru/v8/dt/metadata/mdclass" uuid="cat-uuid">
  <name>Товары</name>
  <forms uuid="form-uuid">
    <name>ФормаСписка</name>
  </forms>
  <commands uuid="cmd-uuid">
    <name>Печать</name>
  </commands>
</mdclass:Catalog>"#,
    );
    write(
        &src.join("Catalogs").join("Товары").join("ObjectModule.bsl"),
        "Процедура ПередЗаписью(Отказ) КонецПроцедуры",
    );
    write(
        &src.join("Catalogs")
            .join("Товары")
            .join("Forms")
            .join("ФормаСписка")
            .join("Module.bsl"),
        "&НаСервере Процедура ПриСозданииНаСервере(Отказ) КонецПроцедуры",
    );
    write(
        &src.join("Catalogs")
            .join("Товары")
            .join("Commands")
            .join("Печать")
            .join("CommandModule.bsl"),
        "Процедура ОбработкаКоманды(Параметр) КонецПроцедуры",
    );
    write(
        &src.join("CommonModules").join("Общий").join("Общий.mdo"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<mdclass:CommonModule xmlns:mdclass="http://g5.1c.ru/v8/dt/metadata/mdclass" uuid="cm-uuid">
  <name>Общий</name>
</mdclass:CommonModule>"#,
    );
    write(
        &src.join("CommonModules").join("Общий").join("Module.bsl"),
        "Процедура Метод() Экспорт КонецПроцедуры",
    );
    // Модуль самой конфигурации владельца-объекта не имеет — как и в формате
    // Конфигуратора, в перечень он попадать не должен.
    write(
        &src.join("Configuration").join("Configuration.mdo"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<mdclass:Configuration xmlns:mdclass="http://g5.1c.ru/v8/dt/metadata/mdclass" uuid="cfg-uuid">
  <name>Конфигурация</name>
</mdclass:Configuration>"#,
    );
    write(
        &src.join("Configuration").join("SessionModule.bsl"),
        "Процедура УстановкаПараметровСеанса(ТребуемыеПараметры) КонецПроцедуры",
    );

    let st = fresh_storage(&tmp);
    index_metadata_modules_edt(tmp.path(), &src, st.conn()).unwrap();

    let mut rows: Vec<(String, String, String, Option<String>)> = st
        .conn()
        .prepare(
            "SELECT full_name, module_type, object_id, config_version \
             FROM metadata_modules WHERE repo = ? ORDER BY full_name",
        )
        .unwrap()
        .query_map(params![REPO_DEFAULT], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows.sort();

    assert_eq!(
        rows.iter()
            .map(|(f, t, id, _)| (f.as_str(), t.as_str(), id.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("Catalogs.Товары.Command.Печать.CommandModule", "CommandModule", "cmd-uuid"),
            ("Catalogs.Товары.Form.ФормаСписка.FormModule", "FormModule", "form-uuid"),
            ("Catalogs.Товары.ObjectModule", "ObjectModule", "cat-uuid"),
            ("CommonModules.Общий.Module", "Module", "cm-uuid"),
        ],
        "форма и команда берут идентификатор из .mdo владельца, модуль конфигурации не в перечне"
    );
    assert!(
        rows.iter().all(|(_, _, _, v)| v.is_none()),
        "версии объекта в EDT нет — колонка остаётся пустой, а не выдуманной"
    );

    let code_path: String = st
        .conn()
        .query_row(
            "SELECT code_path FROM metadata_modules WHERE full_name='Catalogs.Товары.ObjectModule'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        code_path, "src/Catalogs/Товары/ObjectModule.bsl",
        "путь модуля — от корня репозитория, через прямые слеши"
    );
}

#[test]
fn command_named_like_its_object_finds_owner() {
    // Подъём к владельцу шёл до папки С ИМЕНЕМ ОБЪЕКТА, поэтому у команды,
    // названной так же, как сам объект, первой снизу оказывалась папка КОМАНДЫ:
    // владелец искался как `Commands/<Имя>.xml` и не находился (E-12).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");

    // Имя команды совпадает с именем отчёта — самый частый случай.
    let same = repo
        .join("Reports")
        .join("АнализПродаж")
        .join("Commands")
        .join("АнализПродаж")
        .join("Ext")
        .join("CommandModule.bsl");
    write(&same, "Процедура ОбработкаКоманды(П, С) КонецПроцедуры");
    // И команда с собственным именем — проверяем, что прежний случай не сломан.
    let other = repo
        .join("Reports")
        .join("АнализПродаж")
        .join("Commands")
        .join("ПродажиПоКлиентам")
        .join("Ext")
        .join("CommandModule.bsl");
    write(&other, "Процедура ОбработкаКоманды(П, С) КонецПроцедуры");

    write(
        &repo.join("Reports").join("АнализПродаж.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xr="x" xmlns:xsi="y">
  <Report uuid="report-uuid">
    <Properties><Name>АнализПродаж</Name></Properties>
    <ChildObjects>
      <Command uuid="cmd-same">
        <Properties><Name>АнализПродаж</Name></Properties>
      </Command>
      <Command uuid="cmd-other">
        <Properties><Name>ПродажиПоКлиентам</Name></Properties>
      </Command>
    </ChildObjects>
  </Report>
</MetaDataObject>"#,
    );

    let (owner_xml, full_name, command_name) = find_object_command_owner(&same)
        .expect("владелец команды, названной как объект, обязан находиться");
    assert_eq!(command_name, "АнализПродаж");
    assert_eq!(full_name, "Reports.АнализПродаж.Command.АнализПродаж");
    assert_eq!(
        owner_xml,
        repo.join("Reports").join("АнализПродаж.xml"),
        "владельцем считается XML объекта, а не несуществующий файл рядом с папкой команды"
    );

    let (_, full_other, _) = find_object_command_owner(&other).expect("прежний случай не сломан");
    assert_eq!(full_other, "Reports.АнализПродаж.Command.ПродажиПоКлиентам");
}

#[test]
fn common_form_indexed_in_both_dump_formats() {
    // У общей формы нет каталога `Forms` — она сама объект: в формате
    // Конфигуратора `CommonForms/<Имя>/Ext/Form.xml`, в EDT
    // `CommonForms/<Имя>/Form.form`. Под разбор пути «форма внутри объекта»
    // это не подходило, и ни одна общая форма в индекс не попадала (E-3).
    let tmp = TempDir::new().unwrap();

    // ── Формат Конфигуратора ────────────────────────────────────────────
    let repo = tmp.path().join("repo");
    let form_xml = repo
        .join("CommonForms")
        .join("ВыборПериода")
        .join("Ext")
        .join("Form.xml");
    let (owner, form_name) =
        decode_form_path(&repo, &form_xml).expect("общая форма обязана разбираться");
    assert_eq!(owner, "CommonForms.ВыборПериода");
    assert_eq!(form_name, "ВыборПериода");

    // Обычная форма разбирается как раньше.
    let usual = repo
        .join("Documents")
        .join("Реализация")
        .join("Forms")
        .join("ФормаДокумента")
        .join("Ext")
        .join("Form.xml");
    assert_eq!(
        decode_form_path(&repo, &usual),
        Some((
            "Documents.Реализация".to_string(),
            "ФормаДокумента".to_string()
        ))
    );

    // ── Формат EDT ──────────────────────────────────────────────────────
    let src = tmp.path().join("src");
    write(
        &src.join("CommonForms").join("ВыборПериода").join("ВыборПериода.mdo"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<mdclass:CommonForm xmlns:mdclass="http://g5.1c.ru/v8/dt/metadata/mdclass" uuid="cf-uuid">
  <name>ВыборПериода</name>
</mdclass:CommonForm>"#,
    );
    write(
        &src.join("CommonForms").join("ВыборПериода").join("Form.form"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<form:Form xmlns:form="http://g5.1c.ru/v8/dt/form">
  <handlers>
    <event>OnCreateAtServer</event>
    <name>ПриСозданииНаСервере</name>
  </handlers>
</form:Form>"#,
    );

    let st = fresh_storage(&tmp);
    run_edt_metadata_layer(&src, st.conn()).unwrap();

    let (form_name, handlers): (String, String) = st
        .conn()
        .query_row(
            "SELECT form_name, handlers_json FROM metadata_forms \
             WHERE owner_full_name = 'CommonForms.ВыборПериода'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("общая форма EDT обязана попасть в перечень форм");
    assert_eq!(form_name, "ВыборПериода");
    assert!(
        handlers.contains("ПриСозданииНаСервере"),
        "обработчики общей формы читаются: {handlers}"
    );
}
