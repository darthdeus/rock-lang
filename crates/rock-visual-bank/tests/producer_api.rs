use rock_visual_bank::{
    AnimationLine, BANK_FILE_NAME, Camera, DIRECTION_COUNT, JointTrackSet, Line, RELIGHT_CHANNELS,
    SHEET_GEOMETRY, SheetGeometry, SurfaceLine, check_line, check_polygon, finite_point,
    is_sheet_dir, mask_file, validate_floor_texture,
};

#[test]
fn producer_can_share_bank_schema_and_validation() {
    assert_eq!(BANK_FILE_NAME, "visual_bank.jsonl");
    assert_eq!(SHEET_GEOMETRY, "sheet.json");
    assert_eq!(RELIGHT_CHANNELS.len(), 4);
    assert_eq!(DIRECTION_COUNT, 8);

    let _: fn(&Line) -> anyhow::Result<()> = check_line;
    let _: fn(&[[f32; 2]]) -> anyhow::Result<()> = check_polygon;
    let _: fn([f32; 2]) -> bool = finite_point;
    let _: fn(&str) -> bool = is_sheet_dir;
    let _: fn(&str) -> String = mask_file;
    type FloorTextureValidator =
        fn(Option<[u32; 2]>, Option<[u32; 4]>, Option<[u32; 2]>, (u32, u32)) -> anyhow::Result<()>;
    let _: FloorTextureValidator = validate_floor_texture;

    let tracks: JointTrackSet = Default::default();
    let geometry = SheetGeometry {
        rects: Vec::new(),
        joints: tracks,
    };
    assert!(geometry.joints.is_empty());

    let mut line = Line::Animation(AnimationLine {
        albedo_bc1: false,
        character: "person".into(),
        label: "idle".into(),
        dir: "0".repeat(64),
        size: [1, 1],
        cell: [1, 1],
        pivot: [0.5, 0.5],
        frames: 1,
        frame_time: 0.1,
        skin_mask: false,
        travel: None,
    });
    *line.dir_mut().expect("animation owns a sheet") = "1".repeat(64);
    assert_eq!(line.dir(), Some("1".repeat(64).as_str()));

    let character = Line::Character(rock_visual_bank::CharacterLine {
        id: "person".into(),
        draw_size: 1.0,
        padding_px: 0.0,
        layer_of: None,
        camera: Camera {
            ground_y: 0.0,
            bases: vec![[0.0; 12]; DIRECTION_COUNT],
        },
    });
    assert!(character.clone().dir_mut().is_none());

    let _ = SurfaceLine {
        id: "surface".into(),
        dir: "0".repeat(64),
        size: [1, 1],
        cell: [1, 1],
        pivot: [0.5, 0.5],
        camera: Camera {
            ground_y: 0.0,
            bases: vec![[0.0; 12]; DIRECTION_COUNT],
        },
        skin_mask: false,
        studio: None,
    };
}

#[test]
fn compressed_albedo_is_packaged_alongside_the_original() {
    let bank = r#"{"kind":"animation","character":"actor","label":"idle","dir":"0000000000000000000000000000000000000000000000000000000000000000","size":[5,3],"cell":[5,3],"pivot":[0.5,0.5],"frames":1,"frame_time":1.0,"skin_mask":false,"albedo_bc1":true}"#;
    let (rows, errors) = rock_visual_bank::parse_bank(&format!("{bank}\n"));
    assert!(errors.is_empty(), "{errors:?}");
    let files = rock_visual_bank::sheet_files(&rows[0]);
    assert!(files.contains(&"albedo.dds".to_owned()));
    assert!(files.contains(&"albedo.png".to_owned()));
}
