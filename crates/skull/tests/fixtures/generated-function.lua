function dungeon_queue_sprites_SpaceId_DungeonRelightCache_Image_DungeonRelightU_ret_unit(c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15, c16, c17, c18, c19, c20, c21, c22, c23, c24, c25, c26, c27, c28, c29, c30, c31, c32, c33, c34, c35, c36, c37, c38, c39, c40, c41, c42, c43, c44, c45, c46, c47, c48, c49, c50, c51, c52, c53, c54, c55, c56, c57, c58, c59, c60)
  local r0, r1, c61, c62, c63, c64, c65, c66, c67, c68, c69, c70, c71, c72, c73, c74, c75, c76, c77, c78, c79, c80, c81, c82, c83, c84, c85, c86, c87, c88, c89, c90, c91, c92, c93, c94, c95, c96, c97, c98, c99, c100, c101, c102, c103, c104, c105, c106, c107, c108, c109, c110, c111, c112, c113, c114, c115, c116, c117, c118, c119
  ::bb0::
  c61 = (0)
  r0 = __rock_active_session.__rock_state[158]
  for c61 = c61, r0 - 1 do
  local r2, r3, r4, r5, r6, r7, r8, r9, r10, r11, r12, r13, r14, r15, r16, r17, r18, r19, r20, r21, r22, r23, r24, r25, r26, r27, r28, r29, r30, r31, r32, r33, r34, r35, r36, r37, r38, r39, r40, r41, r42, r43, r44, r45, r46, r47, r48, r49, r50, r51, r52, r53, r54, r55, r56, r57, r58, r59, r60, r61, r62, r63, r64, r65
  ::bb2::
  r2 = __rock_active_session.__rock_state[159]
  r3 = c61
  r2 = __vec_checked_get(r2, r3)
  r3 = r2["space.value"]
  if (r3 ~= c0) then goto bb5 else goto bb6 end
  ::bb5::
  goto bb3
  ::bb6::
  goto bb7
  ::bb7::
  r3 = r2.inputs
  r3 = clear_arr_T_ret_unit_spec_t_2_a_n14_SampledTexture_0_z_a_n13_TextureFilter_0_z_z(r3)
  r3 = r2.frame
  r3 = r3.inputs
  c64 = (0)
  goto bb8
  ::bb8::
  goto bb29
  ::bb29::
  r4 = c64
  r5 = r3.len
  if (r4 >= r5) then goto bb30 else goto bb31 end
  ::bb30::
  c62 = (false)
  c63 = (nil)
  goto bb33
  ::bb31::
  goto bb32
  ::bb32::
  r4 = c64
  r4 = __vec_checked_get(r3, r4)
  r5 = c64
  c64 = (r5 + (1))
  c62 = (true)
  c63 = r4
  goto bb33
  ::bb33::
  r4 = c62
  if r4 then goto bb9 else goto bb10 end
  ::bb9::
  r4 = c63
  r5 = r2.inputs
  do
    local vec = r5
    local item = r4
        vec.len = vec.len + 1
        vec._data[vec.len] = item
  end
  goto bb8
  ::bb10::
  r3 = r2.inputs
  c62, c63 = sampled_image_Image_ret_SampledTexture(c2)
  r4 = c62
  r5 = c63
  if (r4 == (nil)) then goto bb11 else goto bb12 end
  ::bb11::
  c62 = (nil)
  goto bb13
  ::bb12::
  c62 = ({__variant = r4, __ty = "SampledTexture", r5})
  goto bb13
  ::bb13::
  r4 = c62
  do
    local vec = r3
    local item = ({r4, (0)})
        vec.len = vec.len + 1
        vec._data[vec.len] = item
  end
  r3 = r2.inputs
  r4 = c1.atlas
  goto bb34
  ::bb34::
  if (r4 ~= (nil)) then goto bb35 else goto bb36 end
  ::bb35::
  c62 = r4
  goto bb38
  ::bb36::
  goto bb37
  ::bb37::
  do
    r4 = debug.traceback("", 2)
  end
  r4 = error((("unwrap() called on null value\n") .. r4))
  c62 = (nil)
  goto bb38
  ::bb38::
  r4 = c62
  c62, c63 = sampled_canvas_Canvas_ret_SampledTexture(r4)
  r4 = c62
  r5 = c63
  if (r4 == (nil)) then goto bb14 else goto bb15 end
  ::bb14::
  c62 = (nil)
  goto bb16
  ::bb15::
  c62 = ({__variant = r4, __ty = "SampledTexture", r5})
  goto bb16
  ::bb16::
  r4 = c62
  do
    local vec = r3
    local item = ({r4, (0)})
        vec.len = vec.len + 1
        vec._data[vec.len] = item
  end
  r3 = r2.inputs
  r4 = c1.sources
  goto bb39
  ::bb39::
  if (r4 ~= (nil)) then goto bb40 else goto bb41 end
  ::bb40::
  c62 = r4
  goto bb43
  ::bb41::
  goto bb42
  ::bb42::
  do
    r4 = debug.traceback("", 2)
  end
  r4 = error((("unwrap() called on null value\n") .. r4))
  c62 = (nil)
  goto bb43
  ::bb43::
  r4 = c62
  c62, c63 = sampled_image_Image_ret_SampledTexture(r4)
  r4 = c62
  r5 = c63
  if (r4 == (nil)) then goto bb17 else goto bb18 end
  ::bb17::
  c62 = (nil)
  goto bb19
  ::bb18::
  c62 = ({__variant = r4, __ty = "SampledTexture", r5})
  goto bb19
  ::bb19::
  r4 = c62
  do
    local vec = r3
    local item = ({r4, (0)})
        vec.len = vec.len + 1
        vec._data[vec.len] = item
  end
  r3 = r2.inputs
  r4 = c1.atlas
  goto bb44
  ::bb44::
  if (r4 ~= (nil)) then goto bb45 else goto bb46 end
  ::bb45::
  c62 = r4
  goto bb48
  ::bb46::
  goto bb47
  ::bb47::
  do
    r4 = debug.traceback("", 2)
  end
  r4 = error((("unwrap() called on null value\n") .. r4))
  c62 = (nil)
  goto bb48
  ::bb48::
  r4 = c62
  c62, c63 = sampled_canvas_Canvas_ret_SampledTexture(r4)
  r4 = c62
  r5 = c63
  if (r4 == (nil)) then goto bb20 else goto bb21 end
  ::bb20::
  c62 = (nil)
  goto bb22
  ::bb21::
  c62 = ({__variant = r4, __ty = "SampledTexture", r5})
  goto bb22
  ::bb22::
  r4 = c62
  do
    local vec = r3
    local item = ({r4, (0)})
        vec.len = vec.len + 1
        vec._data[vec.len] = item
  end
  c62, c63, c64, c65, c66, c67, c68, c69, c70, c71, c72, c73, c74, c75, c76, c77, c78, c79, c80, c81, c82, c83, c84, c85, c86, c87, c88, c89, c90, c91, c92, c93, c94, c95, c96, c97, c98, c99, c100, c101, c102, c103, c104, c105, c106, c107, c108, c109, c110, c111, c112, c113, c114, c115, c116, c117, c118, c119 = dungeon_sprite_uniform_DungeonSpriteDraw_DungeonRelightU_ret_DungeonRelightU(r2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15, c16, c17, c18, c19, c20, c21, c22, c23, c24, c25, c26, c27, c28, c29, c30, c31, c32, c33, c34, c35, c36, c37, c38, c39, c40, c41, c42, c43, c44, c45, c46, c47, c48, c49, c50, c51, c52, c53, c54, c55, c56, c57, c58, c59, c60)
  r3 = c62
  r4 = c63
  r5 = c64
  r6 = c65
  r7 = c66
  r8 = c67
  r9 = c68
  r10 = c69
  r11 = c70
  r12 = c71
  r13 = c72
  r14 = c73
  r15 = c74
  r16 = c75
  r17 = c76
  r18 = c77
  r19 = c78
  r20 = c79
  r21 = c80
  r22 = c81
  r23 = c82
  r24 = c83
  r25 = c84
  r26 = c85
  r27 = c86
  r28 = c87
  r29 = c88
  r30 = c89
  r31 = c90
  r32 = c91
  r33 = c92
  r34 = c93
  r35 = c94
  r36 = c95
  r37 = c96
  r38 = c97
  r39 = c98
  r40 = c99
  r41 = c100
  r42 = c101
  r43 = c102
  r44 = c103
  r45 = c104
  r46 = c105
  r47 = c106
  r48 = c107
  r49 = c108
  r50 = c109
  r51 = c110
  r52 = c111
  r53 = c112
  r54 = c113
  r55 = c114
  r56 = c115
  r57 = c116
  r58 = c117
  r59 = c118
  r60 = c119
  r61 = __rock_active_session.__rock_state[161]
  r62 = r2.inputs
  r63 = __rock_active_session.__rock_state[161]
  r63 = r63.shader_name
  r63 = new_frame_owned_block_string_int_string_ret_OwnedUniformBlock_opt(r63, (1836237630983811), ("fixtures/wide_uniform.rock:111"))
  if (r63 ~= (nil)) then goto bb23 else goto bb24 end
  ::bb23::
  goto bb49
  ::bb49::
  if (r63 ~= (nil)) then goto bb50 else goto bb51 end
  ::bb50::
  c62 = r63
  goto bb53
  ::bb51::
  goto bb52
  ::bb52::
  do
    r64 = debug.traceback("", 2)
  end
  r64 = error((("unwrap() called on null value\n") .. r64))
  c62 = (nil)
  goto bb53
  ::bb53::
  r64 = c62
  r65 = r64.fv
  r65[(0)] = r3
  r65[(1)] = r4
  r65[(2)] = r5
  r65[(3)] = r6
  r65[(4)] = r7
  r65[(5)] = r8
  r65[(6)] = r9
  r65[(7)] = r10
  r65[(8)] = r11
  r65[(9)] = r12
  r65[(10)] = r13
  r65[(11)] = r14
  r65[(12)] = r15
  r65[(13)] = r16
  r65[(14)] = r17
  r65[(16)] = r18
  r65[(17)] = r19
  r65[(18)] = r20
  r65[(20)] = r21
  r65[(21)] = r22
  r65[(22)] = r23
  r65[(24)] = r24
  r65[(25)] = r25
  r65[(26)] = r26
  r65[(28)] = r27
  r65[(29)] = r28
  r65[(30)] = r29
  r65[(32)] = r30
  r65[(33)] = r31
  r65[(34)] = r32
  r65[(36)] = r33
  r65[(37)] = r34
  r65[(38)] = r35
  r65[(40)] = r36
  r65[(41)] = r37
  r65[(42)] = r38
  r65[(44)] = r39
  r65[(45)] = r40
  r65[(46)] = r41
  r65[(48)] = r42
  r65[(49)] = r43
  r65[(50)] = r44
  r65[(52)] = r45
  r65[(53)] = r46
  r65[(54)] = r47
  r65[(56)] = r48
  r65[(57)] = r49
  r65[(58)] = r50
  r65[(60)] = r51
  r65[(61)] = r52
  r65[(62)] = r53
  r65[(64)] = r54
  r65[(65)] = r55
  r65[(66)] = r56
  r65[(67)] = r57
  r65[(68)] = r58
  r65[(69)] = r59
  r65[(70)] = r60
  r65 = host_renderer_is_wgpu_ret_bool()
  if r65 then goto bb26 else goto bb27 end
  ::bb26::
  c62 = ({__ty = "SemanticUniformBlock", fingerprint = (7815447245835893), leaves = ({_data = {({__variant = "Float", __ty = "CanonicalUniformLeaf", r3}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r4}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r5}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r6}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r7}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r8}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r9}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r10}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r11}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r12}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r13}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r14}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r15}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r16}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r17}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r18}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r19}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r20}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r21}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r22}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r23}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r24}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r25}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r26}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r27}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r28}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r29}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r30}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r31}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r32}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r33}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r34}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r35}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r36}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r37}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r38}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r39}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r40}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r41}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r42}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r43}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r44}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r45}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r46}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r47}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r48}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r49}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r50}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r51}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r52}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r53}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r54}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r55}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r56}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r57}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r58}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r59}), ({__variant = "Float", __ty = "CanonicalUniformLeaf", r60})}, len = 58, __ty = "Vec"})})
  goto bb28
  ::bb27::
  c62 = (nil)
  goto bb28
  ::bb28::
  r3 = c62
  r64.semantic = r3
  goto bb25
  ::bb24::
  goto bb25
  ::bb25::
  r2 = r2.output
  r3 = __rock_active_session.__rock_state[1589]
  r4 = __rock_active_session.__rock_state[1590]
  r5 = __rock_active_session.__rock_state[1591]
  r6 = __rock_active_session.__rock_state[1592]
  r7 = __rock_active_session.__rock_state[1611]
  r2 = queue_world_prepare_pass_Pass_ret_unit(({__ty = "Pass", pipeline = r61, fullscreen_v_flip = (false), inputs = ({_data = {}, len = 0, __ty = "Vec"}), slot_inputs = r62, uniforms = r7, block = r63, array_payload = ({_data = {}, len = 0, __ty = "Vec"}), output = r2, ["clear.r"] = r3, ["clear.g"] = r4, ["clear.b"] = r5, ["clear.a"] = r6, clear = (true), blend = (5)}))
  goto bb3
  ::bb3::
  end
  goto bb4
  ::bb4::
  do return end
end
