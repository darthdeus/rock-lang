require("ffi").cdef[[
typedef struct ImDrawChannel ImDrawChannel;
typedef struct ImDrawCmd ImDrawCmd;
typedef struct ImDrawData ImDrawData;
typedef struct ImDrawList ImDrawList;
typedef struct ImDrawListSharedData ImDrawListSharedData;
typedef struct ImDrawListSplitter ImDrawListSplitter;
typedef struct ImDrawVert ImDrawVert;
typedef struct ImFont ImFont;
typedef struct ImFontAtlas ImFontAtlas;
typedef struct ImFontAtlasBuilder ImFontAtlasBuilder;
typedef struct ImFontBaked ImFontBaked;
typedef struct ImFontConfig ImFontConfig;
typedef struct ImFontGlyph ImFontGlyph;
typedef struct ImFontLoader ImFontLoader;
typedef struct ImTextureData ImTextureData;
typedef struct ImTextureRect ImTextureRect;
typedef struct ImGuiContext ImGuiContext;
typedef struct ImGuiIO ImGuiIO;
typedef struct ImGuiInputTextCallbackData ImGuiInputTextCallbackData;
typedef struct ImGuiKeyData ImGuiKeyData;
typedef struct ImGuiSizeCallbackData ImGuiSizeCallbackData;
typedef struct ImGuiStorage ImGuiStorage;
typedef struct ImGuiStoragePair ImGuiStoragePair;
typedef struct ImGuiStyle ImGuiStyle;
typedef struct ImGuiTextBuffer ImGuiTextBuffer;
typedef struct ImGuiViewport ImGuiViewport;
typedef struct ImGuiWindowClass ImGuiWindowClass;
typedef struct ImRect ImRect;
typedef struct ImDrawDataBuilder ImDrawDataBuilder;
typedef struct ImFontAtlasRectEntry ImFontAtlasRectEntry;
typedef struct ImGuiDockNode ImGuiDockNode;
typedef struct ImGuiMenuColumns ImGuiMenuColumns;
typedef struct ImGuiOldColumnData ImGuiOldColumnData;
typedef struct ImGuiOldColumns ImGuiOldColumns;
typedef struct ImGuiTabBar ImGuiTabBar;
typedef struct ImGuiTabItem ImGuiTabItem;
typedef struct ImGuiWindow ImGuiWindow;
typedef struct ImGuiWindowDockStyle ImGuiWindowDockStyle;
typedef struct ImGuiWindowTempData ImGuiWindowTempData;
typedef struct stbrp_node stbrp_node;
typedef unsigned int ImGuiID;
typedef signed char ImS8;
typedef unsigned char ImU8;
typedef signed short ImS16;
typedef unsigned short ImU16;
typedef signed int ImS32;
typedef unsigned int ImU32;
typedef unsigned long long ImU64;
struct ImDrawChannel;
struct ImDrawCmd;
struct ImDrawData;
struct ImDrawList;
struct ImDrawListSharedData;
struct ImDrawListSplitter;
struct ImDrawVert;
struct ImFont;
struct ImFontAtlas;
struct ImFontAtlasBuilder;
struct ImFontBaked;
struct ImFontConfig;
struct ImFontGlyph;
struct ImFontLoader;
struct ImTextureData;
struct ImTextureRect;
struct ImGuiContext;
struct ImGuiIO;
struct ImGuiInputTextCallbackData;
struct ImGuiKeyData;
struct ImGuiSizeCallbackData;
struct ImGuiStorage;
struct ImGuiStoragePair;
struct ImGuiStyle;
struct ImGuiTextBuffer;
struct ImGuiViewport;
struct ImGuiWindowClass;
typedef int ImGuiCol;
typedef int ImGuiCond;
typedef int ImGuiMouseButton;
typedef int ImDrawFlags;
typedef int ImDrawListFlags;
typedef int ImFontFlags;
typedef int ImFontAtlasFlags;
typedef int ImGuiBackendFlags;
typedef int ImGuiButtonFlags;
typedef int ImGuiChildFlags;
typedef int ImGuiColorEditFlags;
typedef int ImGuiConfigFlags;
typedef int ImGuiComboFlags;
typedef int ImGuiDockNodeFlags;
typedef int ImGuiFocusedFlags;
typedef int ImGuiHoveredFlags;
typedef int ImGuiInputTextFlags;
typedef int ImGuiKeyChord;
typedef int ImGuiSelectableFlags;
typedef int ImGuiSliderFlags;
typedef int ImGuiTabBarFlags;
typedef int ImGuiTabItemFlags;
typedef int ImGuiTableFlags;
typedef int ImGuiTableColumnFlags;
typedef int ImGuiTableRowFlags;
typedef int ImGuiTreeNodeFlags;
typedef int ImGuiViewportFlags;
typedef int ImGuiWindowFlags;
typedef unsigned short ImWchar16;
typedef ImWchar16 ImWchar;
typedef int (*ImGuiInputTextCallback)(ImGuiInputTextCallbackData* data);
typedef void (*ImGuiSizeCallback)(ImGuiSizeCallbackData* data);
typedef struct ImVec2 ImVec2;
struct ImVec2
{
    float x, y;
};
typedef struct ImVec4 ImVec4;
struct ImVec4
{
    float x, y, z, w;
};
typedef ImU64 ImTextureID;
typedef struct ImTextureRef ImTextureRef;
struct ImTextureRef
{
    ImTextureData* _TexData;
    ImTextureID _TexID;
};
typedef enum {
ImGuiDir_None=-1,
ImGuiDir_Left=0,
ImGuiDir_Right=1,
ImGuiDir_Up=2,
ImGuiDir_Down=3,
ImGuiDir_COUNT=4,
}ImGuiDir;
typedef enum {
ImGuiKey_None=0,
ImGuiKey_NamedKey_BEGIN=512,
ImGuiKey_Tab=512,
ImGuiKey_LeftArrow=513,
ImGuiKey_RightArrow=514,
ImGuiKey_UpArrow=515,
ImGuiKey_DownArrow=516,
ImGuiKey_PageUp=517,
ImGuiKey_PageDown=518,
ImGuiKey_Home=519,
ImGuiKey_End=520,
ImGuiKey_Insert=521,
ImGuiKey_Delete=522,
ImGuiKey_Backspace=523,
ImGuiKey_Space=524,
ImGuiKey_Enter=525,
ImGuiKey_Escape=526,
ImGuiKey_LeftCtrl=527,
ImGuiKey_LeftShift=528,
ImGuiKey_LeftAlt=529,
ImGuiKey_LeftSuper=530,
ImGuiKey_RightCtrl=531,
ImGuiKey_RightShift=532,
ImGuiKey_RightAlt=533,
ImGuiKey_RightSuper=534,
ImGuiKey_Menu=535,
ImGuiKey_0=536,
ImGuiKey_1=537,
ImGuiKey_2=538,
ImGuiKey_3=539,
ImGuiKey_4=540,
ImGuiKey_5=541,
ImGuiKey_6=542,
ImGuiKey_7=543,
ImGuiKey_8=544,
ImGuiKey_9=545,
ImGuiKey_A=546,
ImGuiKey_B=547,
ImGuiKey_C=548,
ImGuiKey_D=549,
ImGuiKey_E=550,
ImGuiKey_F=551,
ImGuiKey_G=552,
ImGuiKey_H=553,
ImGuiKey_I=554,
ImGuiKey_J=555,
ImGuiKey_K=556,
ImGuiKey_L=557,
ImGuiKey_M=558,
ImGuiKey_N=559,
ImGuiKey_O=560,
ImGuiKey_P=561,
ImGuiKey_Q=562,
ImGuiKey_R=563,
ImGuiKey_S=564,
ImGuiKey_T=565,
ImGuiKey_U=566,
ImGuiKey_V=567,
ImGuiKey_W=568,
ImGuiKey_X=569,
ImGuiKey_Y=570,
ImGuiKey_Z=571,
ImGuiKey_F1=572,
ImGuiKey_F2=573,
ImGuiKey_F3=574,
ImGuiKey_F4=575,
ImGuiKey_F5=576,
ImGuiKey_F6=577,
ImGuiKey_F7=578,
ImGuiKey_F8=579,
ImGuiKey_F9=580,
ImGuiKey_F10=581,
ImGuiKey_F11=582,
ImGuiKey_F12=583,
ImGuiKey_F13=584,
ImGuiKey_F14=585,
ImGuiKey_F15=586,
ImGuiKey_F16=587,
ImGuiKey_F17=588,
ImGuiKey_F18=589,
ImGuiKey_F19=590,
ImGuiKey_F20=591,
ImGuiKey_F21=592,
ImGuiKey_F22=593,
ImGuiKey_F23=594,
ImGuiKey_F24=595,
ImGuiKey_Apostrophe=596,
ImGuiKey_Comma=597,
ImGuiKey_Minus=598,
ImGuiKey_Period=599,
ImGuiKey_Slash=600,
ImGuiKey_Semicolon=601,
ImGuiKey_Equal=602,
ImGuiKey_LeftBracket=603,
ImGuiKey_Backslash=604,
ImGuiKey_RightBracket=605,
ImGuiKey_GraveAccent=606,
ImGuiKey_CapsLock=607,
ImGuiKey_ScrollLock=608,
ImGuiKey_NumLock=609,
ImGuiKey_PrintScreen=610,
ImGuiKey_Pause=611,
ImGuiKey_Keypad0=612,
ImGuiKey_Keypad1=613,
ImGuiKey_Keypad2=614,
ImGuiKey_Keypad3=615,
ImGuiKey_Keypad4=616,
ImGuiKey_Keypad5=617,
ImGuiKey_Keypad6=618,
ImGuiKey_Keypad7=619,
ImGuiKey_Keypad8=620,
ImGuiKey_Keypad9=621,
ImGuiKey_KeypadDecimal=622,
ImGuiKey_KeypadDivide=623,
ImGuiKey_KeypadMultiply=624,
ImGuiKey_KeypadSubtract=625,
ImGuiKey_KeypadAdd=626,
ImGuiKey_KeypadEnter=627,
ImGuiKey_KeypadEqual=628,
ImGuiKey_AppBack=629,
ImGuiKey_AppForward=630,
ImGuiKey_Oem102=631,
ImGuiKey_GamepadStart=632,
ImGuiKey_GamepadBack=633,
ImGuiKey_GamepadFaceLeft=634,
ImGuiKey_GamepadFaceRight=635,
ImGuiKey_GamepadFaceUp=636,
ImGuiKey_GamepadFaceDown=637,
ImGuiKey_GamepadDpadLeft=638,
ImGuiKey_GamepadDpadRight=639,
ImGuiKey_GamepadDpadUp=640,
ImGuiKey_GamepadDpadDown=641,
ImGuiKey_GamepadL1=642,
ImGuiKey_GamepadR1=643,
ImGuiKey_GamepadL2=644,
ImGuiKey_GamepadR2=645,
ImGuiKey_GamepadL3=646,
ImGuiKey_GamepadR3=647,
ImGuiKey_GamepadLStickLeft=648,
ImGuiKey_GamepadLStickRight=649,
ImGuiKey_GamepadLStickUp=650,
ImGuiKey_GamepadLStickDown=651,
ImGuiKey_GamepadRStickLeft=652,
ImGuiKey_GamepadRStickRight=653,
ImGuiKey_GamepadRStickUp=654,
ImGuiKey_GamepadRStickDown=655,
ImGuiKey_MouseLeft=656,
ImGuiKey_MouseRight=657,
ImGuiKey_MouseMiddle=658,
ImGuiKey_MouseX1=659,
ImGuiKey_MouseX2=660,
ImGuiKey_MouseWheelX=661,
ImGuiKey_MouseWheelY=662,
ImGuiKey_ReservedForModCtrl=663,
ImGuiKey_ReservedForModShift=664,
ImGuiKey_ReservedForModAlt=665,
ImGuiKey_ReservedForModSuper=666,
ImGuiKey_NamedKey_END=667,
ImGuiKey_NamedKey_COUNT=ImGuiKey_NamedKey_END - ImGuiKey_NamedKey_BEGIN,
ImGuiMod_None=0,
ImGuiMod_Ctrl=1 << 12,
ImGuiMod_Shift=1 << 13,
ImGuiMod_Alt=1 << 14,
ImGuiMod_Super=1 << 15,
ImGuiMod_Mask_=0xF000,
}ImGuiKey;
typedef enum {
    ImGuiCol_Text,
    ImGuiCol_TextDisabled,
    ImGuiCol_WindowBg,
    ImGuiCol_ChildBg,
    ImGuiCol_PopupBg,
    ImGuiCol_Border,
    ImGuiCol_BorderShadow,
    ImGuiCol_FrameBg,
    ImGuiCol_FrameBgHovered,
    ImGuiCol_FrameBgActive,
    ImGuiCol_TitleBg,
    ImGuiCol_TitleBgActive,
    ImGuiCol_TitleBgCollapsed,
    ImGuiCol_MenuBarBg,
    ImGuiCol_ScrollbarBg,
    ImGuiCol_ScrollbarGrab,
    ImGuiCol_ScrollbarGrabHovered,
    ImGuiCol_ScrollbarGrabActive,
    ImGuiCol_CheckMark,
    ImGuiCol_SliderGrab,
    ImGuiCol_SliderGrabActive,
    ImGuiCol_Button,
    ImGuiCol_ButtonHovered,
    ImGuiCol_ButtonActive,
    ImGuiCol_Header,
    ImGuiCol_HeaderHovered,
    ImGuiCol_HeaderActive,
    ImGuiCol_Separator,
    ImGuiCol_SeparatorHovered,
    ImGuiCol_SeparatorActive,
    ImGuiCol_ResizeGrip,
    ImGuiCol_ResizeGripHovered,
    ImGuiCol_ResizeGripActive,
    ImGuiCol_InputTextCursor,
    ImGuiCol_TabHovered,
    ImGuiCol_Tab,
    ImGuiCol_TabSelected,
    ImGuiCol_TabSelectedOverline,
    ImGuiCol_TabDimmed,
    ImGuiCol_TabDimmedSelected,
    ImGuiCol_TabDimmedSelectedOverline,
    ImGuiCol_DockingPreview,
    ImGuiCol_DockingEmptyBg,
    ImGuiCol_PlotLines,
    ImGuiCol_PlotLinesHovered,
    ImGuiCol_PlotHistogram,
    ImGuiCol_PlotHistogramHovered,
    ImGuiCol_TableHeaderBg,
    ImGuiCol_TableBorderStrong,
    ImGuiCol_TableBorderLight,
    ImGuiCol_TableRowBg,
    ImGuiCol_TableRowBgAlt,
    ImGuiCol_TextLink,
    ImGuiCol_TextSelectedBg,
    ImGuiCol_TreeLines,
    ImGuiCol_DragDropTarget,
    ImGuiCol_NavCursor,
    ImGuiCol_NavWindowingHighlight,
    ImGuiCol_NavWindowingDimBg,
    ImGuiCol_ModalWindowDimBg,
    ImGuiCol_COUNT,
}ImGuiCol_;
typedef enum {
ImGuiMouseSource_Mouse=0,
ImGuiMouseSource_TouchScreen=1,
ImGuiMouseSource_Pen=2,
ImGuiMouseSource_COUNT=3,
}ImGuiMouseSource;
struct ImGuiStyle
{
    float FontSizeBase;
    float FontScaleMain;
    float FontScaleDpi;
    float Alpha;
    float DisabledAlpha;
    ImVec2 WindowPadding;
    float WindowRounding;
    float WindowBorderSize;
    float WindowBorderHoverPadding;
    ImVec2 WindowMinSize;
    ImVec2 WindowTitleAlign;
    ImGuiDir WindowMenuButtonPosition;
    float ChildRounding;
    float ChildBorderSize;
    float PopupRounding;
    float PopupBorderSize;
    ImVec2 FramePadding;
    float FrameRounding;
    float FrameBorderSize;
    ImVec2 ItemSpacing;
    ImVec2 ItemInnerSpacing;
    ImVec2 CellPadding;
    ImVec2 TouchExtraPadding;
    float IndentSpacing;
    float ColumnsMinSpacing;
    float ScrollbarSize;
    float ScrollbarRounding;
    float ScrollbarPadding;
    float GrabMinSize;
    float GrabRounding;
    float LogSliderDeadzone;
    float ImageBorderSize;
    float TabRounding;
    float TabBorderSize;
    float TabMinWidthBase;
    float TabMinWidthShrink;
    float TabCloseButtonMinWidthSelected;
    float TabCloseButtonMinWidthUnselected;
    float TabBarBorderSize;
    float TabBarOverlineSize;
    float TableAngledHeadersAngle;
    ImVec2 TableAngledHeadersTextAlign;
    ImGuiTreeNodeFlags TreeLinesFlags;
    float TreeLinesSize;
    float TreeLinesRounding;
    ImGuiDir ColorButtonPosition;
    ImVec2 ButtonTextAlign;
    ImVec2 SelectableTextAlign;
    float SeparatorTextBorderSize;
    ImVec2 SeparatorTextAlign;
    ImVec2 SeparatorTextPadding;
    ImVec2 DisplayWindowPadding;
    ImVec2 DisplaySafeAreaPadding;
    _Bool DockingNodeHasCloseButton;
    float DockingSeparatorSize;
    float MouseCursorScale;
    _Bool AntiAliasedLines;
    _Bool AntiAliasedLinesUseTex;
    _Bool AntiAliasedFill;
    float CurveTessellationTol;
    float CircleTessellationMaxError;
    ImVec4 Colors[ImGuiCol_COUNT];
    float HoverStationaryDelay;
    float HoverDelayShort;
    float HoverDelayNormal;
    ImGuiHoveredFlags HoverFlagsForTooltipMouse;
    ImGuiHoveredFlags HoverFlagsForTooltipNav;
    float _MainScale;
    float _NextFrameFontSizeBase;
};
struct ImGuiKeyData
{
    _Bool Down;
    float DownDuration;
    float DownDurationPrev;
    float AnalogValue;
};
typedef struct ImVector_ImWchar {int Size;int Capacity;ImWchar* Data;} ImVector_ImWchar;
struct ImGuiIO
{
    ImGuiConfigFlags ConfigFlags;
    ImGuiBackendFlags BackendFlags;
    ImVec2 DisplaySize;
    ImVec2 DisplayFramebufferScale;
    float DeltaTime;
    float IniSavingRate;
    const char* IniFilename;
    const char* LogFilename;
    void* UserData;
    ImFontAtlas*Fonts;
    ImFont* FontDefault;
    _Bool FontAllowUserScaling;
    _Bool ConfigNavSwapGamepadButtons;
    _Bool ConfigNavMoveSetMousePos;
    _Bool ConfigNavCaptureKeyboard;
    _Bool ConfigNavEscapeClearFocusItem;
    _Bool ConfigNavEscapeClearFocusWindow;
    _Bool ConfigNavCursorVisibleAuto;
    _Bool ConfigNavCursorVisibleAlways;
    _Bool ConfigDockingNoSplit;
    _Bool ConfigDockingWithShift;
    _Bool ConfigDockingAlwaysTabBar;
    _Bool ConfigDockingTransparentPayload;
    _Bool ConfigViewportsNoAutoMerge;
    _Bool ConfigViewportsNoTaskBarIcon;
    _Bool ConfigViewportsNoDecoration;
    _Bool ConfigViewportsNoDefaultParent;
    _Bool ConfigViewportPlatformFocusSetsImGuiFocus;
    _Bool ConfigDpiScaleFonts;
    _Bool ConfigDpiScaleViewports;
    _Bool MouseDrawCursor;
    _Bool ConfigMacOSXBehaviors;
    _Bool ConfigInputTrickleEventQueue;
    _Bool ConfigInputTextCursorBlink;
    _Bool ConfigInputTextEnterKeepActive;
    _Bool ConfigDragClickToInputText;
    _Bool ConfigWindowsResizeFromEdges;
    _Bool ConfigWindowsMoveFromTitleBarOnly;
    _Bool ConfigWindowsCopyContentsWithCtrlC;
    _Bool ConfigScrollbarScrollByPage;
    float ConfigMemoryCompactTimer;
    float MouseDoubleClickTime;
    float MouseDoubleClickMaxDist;
    float MouseDragThreshold;
    float KeyRepeatDelay;
    float KeyRepeatRate;
    _Bool ConfigErrorRecovery;
    _Bool ConfigErrorRecoveryEnableAssert;
    _Bool ConfigErrorRecoveryEnableDebugLog;
    _Bool ConfigErrorRecoveryEnableTooltip;
    _Bool ConfigDebugIsDebuggerPresent;
    _Bool ConfigDebugHighlightIdConflicts;
    _Bool ConfigDebugHighlightIdConflictsShowItemPicker;
    _Bool ConfigDebugBeginReturnValueOnce;
    _Bool ConfigDebugBeginReturnValueLoop;
    _Bool ConfigDebugIgnoreFocusLoss;
    _Bool ConfigDebugIniSettings;
    const char* BackendPlatformName;
    const char* BackendRendererName;
    void* BackendPlatformUserData;
    void* BackendRendererUserData;
    void* BackendLanguageUserData;
    _Bool WantCaptureMouse;
    _Bool WantCaptureKeyboard;
    _Bool WantTextInput;
    _Bool WantSetMousePos;
    _Bool WantSaveIniSettings;
    _Bool NavActive;
    _Bool NavVisible;
    float Framerate;
    int MetricsRenderVertices;
    int MetricsRenderIndices;
    int MetricsRenderWindows;
    int MetricsActiveWindows;
    ImVec2 MouseDelta;
    ImGuiContext* Ctx;
    ImVec2 MousePos;
    _Bool MouseDown[5];
    float MouseWheel;
    float MouseWheelH;
    ImGuiMouseSource MouseSource;
    ImGuiID MouseHoveredViewport;
    _Bool KeyCtrl;
    _Bool KeyShift;
    _Bool KeyAlt;
    _Bool KeySuper;
    ImGuiKeyChord KeyMods;
    ImGuiKeyData KeysData[ImGuiKey_NamedKey_COUNT];
    _Bool WantCaptureMouseUnlessPopupClose;
    ImVec2 MousePosPrev;
    ImVec2 MouseClickedPos[5];
    double MouseClickedTime[5];
    _Bool MouseClicked[5];
    _Bool MouseDoubleClicked[5];
    ImU16 MouseClickedCount[5];
    ImU16 MouseClickedLastCount[5];
    _Bool MouseReleased[5];
    double MouseReleasedTime[5];
    _Bool MouseDownOwned[5];
    _Bool MouseDownOwnedUnlessPopupClose[5];
    _Bool MouseWheelRequestAxisSwap;
    _Bool MouseCtrlLeftAsRightClick;
    float MouseDownDuration[5];
    float MouseDownDurationPrev[5];
    ImVec2 MouseDragMaxDistanceAbs[5];
    float MouseDragMaxDistanceSqr[5];
    float PenPressure;
    _Bool AppFocusLost;
    _Bool AppAcceptingEvents;
    ImWchar16 InputQueueSurrogate;
    ImVector_ImWchar InputQueueCharacters;
};
struct ImGuiInputTextCallbackData
{
    ImGuiContext* Ctx;
    ImGuiInputTextFlags EventFlag;
    ImGuiInputTextFlags Flags;
    void* UserData;
    ImWchar EventChar;
    ImGuiKey EventKey;
    char* Buf;
    int BufTextLen;
    int BufSize;
    _Bool BufDirty;
    int CursorPos;
    int SelectionStart;
    int SelectionEnd;
};
struct ImGuiSizeCallbackData
{
    void* UserData;
    ImVec2 Pos;
    ImVec2 CurrentSize;
    ImVec2 DesiredSize;
};
struct ImGuiWindowClass
{
    ImGuiID ClassId;
    ImGuiID ParentViewportId;
    ImGuiID FocusRouteParentWindowId;
    ImGuiViewportFlags ViewportFlagsOverrideSet;
    ImGuiViewportFlags ViewportFlagsOverrideClear;
    ImGuiTabItemFlags TabItemFlagsOverrideSet;
    ImGuiDockNodeFlags DockNodeFlagsOverrideSet;
    _Bool DockingAlwaysTabBar;
    _Bool DockingAllowUnclassed;
};
typedef struct ImVector_char {int Size;int Capacity;char* Data;} ImVector_char;
struct ImGuiTextBuffer
{
    ImVector_char Buf;
};
struct ImGuiStoragePair
{
    ImGuiID key;
    union { int val_i; float val_f; void* val_p; };
};
typedef struct ImVector_ImGuiStoragePair {int Size;int Capacity;ImGuiStoragePair* Data;} ImVector_ImGuiStoragePair;
struct ImGuiStorage
{
    ImVector_ImGuiStoragePair Data;
};
typedef unsigned short ImDrawIdx;
typedef void (*ImDrawCallback)(const ImDrawList* parent_list, const ImDrawCmd* cmd);
struct ImDrawCmd
{
    ImVec4 ClipRect;
    ImTextureRef TexRef;
    unsigned int VtxOffset;
    unsigned int IdxOffset;
    unsigned int ElemCount;
    ImDrawCallback UserCallback;
    void* UserCallbackData;
    int UserCallbackDataSize;
    int UserCallbackDataOffset;
};
struct ImDrawVert
{
    ImVec2 pos;
    ImVec2 uv;
    ImU32 col;
};
typedef struct ImDrawCmdHeader ImDrawCmdHeader;
struct ImDrawCmdHeader
{
    ImVec4 ClipRect;
    ImTextureRef TexRef;
    unsigned int VtxOffset;
};
typedef struct ImVector_ImDrawCmd {int Size;int Capacity;ImDrawCmd* Data;} ImVector_ImDrawCmd;
typedef struct ImVector_ImDrawIdx {int Size;int Capacity;ImDrawIdx* Data;} ImVector_ImDrawIdx;
struct ImDrawChannel
{
    ImVector_ImDrawCmd _CmdBuffer;
    ImVector_ImDrawIdx _IdxBuffer;
};
typedef struct ImVector_ImDrawChannel {int Size;int Capacity;ImDrawChannel* Data;} ImVector_ImDrawChannel;
struct ImDrawListSplitter
{
    int _Current;
    int _Count;
    ImVector_ImDrawChannel _Channels;
};
typedef struct ImVector_ImDrawVert {int Size;int Capacity;ImDrawVert* Data;} ImVector_ImDrawVert;
typedef struct ImVector_ImVec2 {int Size;int Capacity;ImVec2* Data;} ImVector_ImVec2;
typedef struct ImVector_ImVec4 {int Size;int Capacity;ImVec4* Data;} ImVector_ImVec4;
typedef struct ImVector_ImTextureRef {int Size;int Capacity;ImTextureRef* Data;} ImVector_ImTextureRef;
typedef struct ImVector_ImU8 {int Size;int Capacity;ImU8* Data;} ImVector_ImU8;
struct ImDrawList
{
    ImVector_ImDrawCmd CmdBuffer;
    ImVector_ImDrawIdx IdxBuffer;
    ImVector_ImDrawVert VtxBuffer;
    ImDrawListFlags Flags;
    unsigned int _VtxCurrentIdx;
    ImDrawListSharedData* _Data;
    ImDrawVert* _VtxWritePtr;
    ImDrawIdx* _IdxWritePtr;
    ImVector_ImVec2 _Path;
    ImDrawCmdHeader _CmdHeader;
    ImDrawListSplitter _Splitter;
    ImVector_ImVec4 _ClipRectStack;
    ImVector_ImTextureRef _TextureStack;
    ImVector_ImU8 _CallbacksDataBuf;
    float _FringeScale;
    const char* _OwnerName;
};
typedef struct ImVector_ImDrawListPtr {int Size;int Capacity;ImDrawList** Data;} ImVector_ImDrawListPtr;
typedef struct ImVector_ImTextureDataPtr {int Size;int Capacity;ImTextureData** Data;} ImVector_ImTextureDataPtr;
struct ImDrawData
{
    _Bool Valid;
    int CmdListsCount;
    int TotalIdxCount;
    int TotalVtxCount;
    ImVector_ImDrawListPtr CmdLists;
    ImVec2 DisplayPos;
    ImVec2 DisplaySize;
    ImVec2 FramebufferScale;
    ImGuiViewport* OwnerViewport;
    ImVector_ImTextureDataPtr* Textures;
};
typedef enum {
    ImTextureFormat_RGBA32,
    ImTextureFormat_Alpha8,
}ImTextureFormat;
typedef enum {
    ImTextureStatus_OK,
    ImTextureStatus_Destroyed,
    ImTextureStatus_WantCreate,
    ImTextureStatus_WantUpdates,
    ImTextureStatus_WantDestroy,
}ImTextureStatus;
struct ImTextureRect
{
    unsigned short x, y;
    unsigned short w, h;
};
typedef struct ImVector_ImTextureRect {int Size;int Capacity;ImTextureRect* Data;} ImVector_ImTextureRect;
struct ImTextureData
{
    int UniqueID;
    ImTextureStatus Status;
    void* BackendUserData;
    ImTextureID TexID;
    ImTextureFormat Format;
    int Width;
    int Height;
    int BytesPerPixel;
    unsigned char* Pixels;
    ImTextureRect UsedRect;
    ImTextureRect UpdateRect;
    ImVector_ImTextureRect Updates;
    int UnusedFrames;
    unsigned short RefCount;
    _Bool UseColors;
    _Bool WantDestroyNextFrame;
};
struct ImFontConfig
{
    char Name[40];
    void* FontData;
    int FontDataSize;
    _Bool FontDataOwnedByAtlas;
    _Bool MergeMode;
    _Bool PixelSnapH;
    _Bool PixelSnapV;
    ImS8 OversampleH;
    ImS8 OversampleV;
    ImWchar EllipsisChar;
    float SizePixels;
    const ImWchar* GlyphRanges;
    const ImWchar* GlyphExcludeRanges;
    ImVec2 GlyphOffset;
    float GlyphMinAdvanceX;
    float GlyphMaxAdvanceX;
    float GlyphExtraAdvanceX;
    ImU32 FontNo;
    unsigned int FontLoaderFlags;
    float RasterizerMultiply;
    float RasterizerDensity;
    ImFontFlags Flags;
    ImFont* DstFont;
    const ImFontLoader* FontLoader;
    void* FontLoaderData;
};
struct ImFontGlyph
{
    unsigned int Colored : 1;
    unsigned int Visible : 1;
    unsigned int SourceIdx : 4;
    unsigned int Codepoint : 26;
    float AdvanceX;
    float X0, Y0, X1, Y1;
    float U0, V0, U1, V1;
    int PackId;
};
typedef int ImFontAtlasRectId;
typedef struct ImVector_ImFontPtr {int Size;int Capacity;ImFont** Data;} ImVector_ImFontPtr;
typedef struct ImVector_ImFontConfig {int Size;int Capacity;ImFontConfig* Data;} ImVector_ImFontConfig;
typedef struct ImVector_ImDrawListSharedDataPtr {int Size;int Capacity;ImDrawListSharedData** Data;} ImVector_ImDrawListSharedDataPtr;
struct ImFontAtlas
{
    ImFontAtlasFlags Flags;
    ImTextureFormat TexDesiredFormat;
    int TexGlyphPadding;
    int TexMinWidth;
    int TexMinHeight;
    int TexMaxWidth;
    int TexMaxHeight;
    void* UserData;
    ImTextureRef TexRef;
    ImTextureData* TexData;
    ImVector_ImTextureDataPtr TexList;
    _Bool Locked;
    _Bool RendererHasTextures;
    _Bool TexIsBuilt;
    _Bool TexPixelsUseColors;
    ImVec2 TexUvScale;
    ImVec2 TexUvWhitePixel;
    ImVector_ImFontPtr Fonts;
    ImVector_ImFontConfig Sources;
    ImVec4 TexUvLines[(32) + 1];
    int TexNextUniqueID;
    int FontNextUniqueID;
    ImVector_ImDrawListSharedDataPtr DrawListSharedDatas;
    ImFontAtlasBuilder* Builder;
    const ImFontLoader* FontLoader;
    const char* FontLoaderName;
    void* FontLoaderData;
    unsigned int FontLoaderFlags;
    int RefCount;
    ImGuiContext* OwnerContext;
};
typedef struct ImVector_float {int Size;int Capacity;float* Data;} ImVector_float;
typedef struct ImVector_ImU16 {int Size;int Capacity;ImU16* Data;} ImVector_ImU16;
typedef struct ImVector_ImFontGlyph {int Size;int Capacity;ImFontGlyph* Data;} ImVector_ImFontGlyph;
struct ImFontBaked
{
    ImVector_float IndexAdvanceX;
    float FallbackAdvanceX;
    float Size;
    float RasterizerDensity;
    ImVector_ImU16 IndexLookup;
    ImVector_ImFontGlyph Glyphs;
    int FallbackGlyphIndex;
    float Ascent, Descent;
    unsigned int MetricsTotalSurface:26;
    unsigned int WantDestroy:1;
    unsigned int LoadNoFallback:1;
    unsigned int LoadNoRenderOnLayout:1;
    int LastUsedFrame;
    ImGuiID BakedId;
    ImFont* ContainerFont;
    void* FontLoaderDatas;
};
typedef struct ImVector_ImFontConfigPtr {int Size;int Capacity;ImFontConfig** Data;} ImVector_ImFontConfigPtr;
struct ImFont
{
    ImFontBaked* LastBaked;
    ImFontAtlas* ContainerAtlas;
    ImFontFlags Flags;
    float CurrentRasterizerDensity;
    ImGuiID FontId;
    float LegacySize;
    ImVector_ImFontConfigPtr Sources;
    ImWchar EllipsisChar;
    ImWchar FallbackChar;
    ImU8 Used8kPagesMap[(0xFFFF +1)/8192/8];
    _Bool EllipsisAutoBake;
    ImGuiStorage RemapPairs;
};
struct ImGuiViewport
{
    ImGuiID ID;
    ImGuiViewportFlags Flags;
    ImVec2 Pos;
    ImVec2 Size;
    ImVec2 FramebufferScale;
    ImVec2 WorkPos;
    ImVec2 WorkSize;
    float DpiScale;
    ImGuiID ParentViewportId;
    ImDrawData* DrawData;
    void* RendererUserData;
    void* PlatformUserData;
    void* PlatformHandle;
    void* PlatformHandleRaw;
    _Bool PlatformWindowCreated;
    _Bool PlatformRequestMove;
    _Bool PlatformRequestResize;
    _Bool PlatformRequestClose;
};
struct ImRect;
struct ImDrawDataBuilder;
struct ImDrawListSharedData;
struct ImFontAtlasBuilder;
struct ImFontAtlasRectEntry;
struct ImGuiContext;
struct ImGuiDockNode;
struct ImGuiMenuColumns;
struct ImGuiOldColumnData;
struct ImGuiOldColumns;
struct ImGuiTabBar;
struct ImGuiTabItem;
struct ImGuiWindow;
struct ImGuiWindowDockStyle;
struct ImGuiWindowTempData;
typedef int ImGuiDataAuthority;
typedef int ImGuiLayoutType;
typedef int ImGuiItemStatusFlags;
typedef int ImGuiOldColumnFlags;
typedef struct ImVec1 ImVec1;
struct ImVec1
{
    float x;
};
typedef struct ImVec2i ImVec2i;
struct ImVec2i
{
    int x, y;
};
typedef struct ImVec2ih ImVec2ih;
struct ImVec2ih
{
    short x, y;
};
struct ImRect
{
    ImVec2 Min;
    ImVec2 Max;
};
struct ImDrawListSharedData
{
    ImVec2 TexUvWhitePixel;
    const ImVec4* TexUvLines;
    ImFontAtlas* FontAtlas;
    ImFont* Font;
    float FontSize;
    float FontScale;
    float CurveTessellationTol;
    float CircleSegmentMaxError;
    float InitialFringeScale;
    ImDrawListFlags InitialFlags;
    ImVec4 ClipRectFullscreen;
    ImVector_ImVec2 TempBuffer;
    ImVector_ImDrawListPtr DrawLists;
    ImGuiContext* Context;
    ImVec2 ArcFastVtx[48];
    float ArcFastRadiusCutoff;
    ImU8 CircleSegmentCounts[64];
};
struct ImDrawDataBuilder
{
    ImVector_ImDrawListPtr* Layers[2];
    ImVector_ImDrawListPtr LayerData1;
};
typedef enum {
    ImGuiAxis_None = -1,
    ImGuiAxis_X = 0,
    ImGuiAxis_Y = 1
}ImGuiAxis;
struct ImGuiMenuColumns
{
    ImU32 TotalWidth;
    ImU32 NextTotalWidth;
    ImU16 Spacing;
    ImU16 OffsetIcon;
    ImU16 OffsetLabel;
    ImU16 OffsetShortcut;
    ImU16 OffsetMark;
    ImU16 Widths[4];
};
typedef enum {
    ImGuiNavLayer_Main = 0,
    ImGuiNavLayer_Menu = 1,
    ImGuiNavLayer_COUNT
}ImGuiNavLayer;
struct ImGuiOldColumnData
{
    float OffsetNorm;
    float OffsetNormBeforeResize;
    ImGuiOldColumnFlags Flags;
    ImRect ClipRect;
};
typedef struct ImVector_ImGuiOldColumnData {int Size;int Capacity;ImGuiOldColumnData* Data;} ImVector_ImGuiOldColumnData;
struct ImGuiOldColumns
{
    ImGuiID ID;
    ImGuiOldColumnFlags Flags;
    _Bool IsFirstFrame;
    _Bool IsBeingResized;
    int Current;
    int Count;
    float OffMinX, OffMaxX;
    float LineMinY, LineMaxY;
    float HostCursorPosY;
    float HostCursorMaxPosX;
    ImRect HostInitialClipRect;
    ImRect HostBackupClipRect;
    ImRect HostBackupParentWorkRect;
    ImVector_ImGuiOldColumnData Columns;
    ImDrawListSplitter Splitter;
};
typedef enum {
    ImGuiDockNodeState_Unknown,
    ImGuiDockNodeState_HostWindowHiddenBecauseSingleWindow,
    ImGuiDockNodeState_HostWindowHiddenBecauseWindowsAreResizing,
    ImGuiDockNodeState_HostWindowVisible,
}ImGuiDockNodeState;
typedef struct ImVector_ImGuiWindowPtr {int Size;int Capacity;ImGuiWindow** Data;} ImVector_ImGuiWindowPtr;
struct ImGuiDockNode
{
    ImGuiID ID;
    ImGuiDockNodeFlags SharedFlags;
    ImGuiDockNodeFlags LocalFlags;
    ImGuiDockNodeFlags LocalFlagsInWindows;
    ImGuiDockNodeFlags MergedFlags;
    ImGuiDockNodeState State;
    ImGuiDockNode* ParentNode;
    ImGuiDockNode* ChildNodes[2];
    ImVector_ImGuiWindowPtr Windows;
    ImGuiTabBar* TabBar;
    ImVec2 Pos;
    ImVec2 Size;
    ImVec2 SizeRef;
    ImGuiAxis SplitAxis;
    ImGuiWindowClass WindowClass;
    ImU32 LastBgColor;
    ImGuiWindow* HostWindow;
    ImGuiWindow* VisibleWindow;
    ImGuiDockNode* CentralNode;
    ImGuiDockNode* OnlyNodeWithWindows;
    int CountNodeWithWindows;
    int LastFrameAlive;
    int LastFrameActive;
    int LastFrameFocused;
    ImGuiID LastFocusedNodeId;
    ImGuiID SelectedTabId;
    ImGuiID WantCloseTabId;
    ImGuiID RefViewportId;
    ImGuiDataAuthority AuthorityForPos :3;
    ImGuiDataAuthority AuthorityForSize :3;
    ImGuiDataAuthority AuthorityForViewport :3;
    _Bool IsVisible :1;
    _Bool IsFocused :1;
    _Bool IsBgDrawnThisFrame :1;
    _Bool HasCloseButton :1;
    _Bool HasWindowMenuButton :1;
    _Bool HasCentralNodeChild :1;
    _Bool WantCloseAll :1;
    _Bool WantLockSizeOnce :1;
    _Bool WantMouseMove :1;
    _Bool WantHiddenTabBarUpdate :1;
    _Bool WantHiddenTabBarToggle :1;
};
typedef enum {
    ImGuiWindowDockStyleCol_Text,
    ImGuiWindowDockStyleCol_TabHovered,
    ImGuiWindowDockStyleCol_TabFocused,
    ImGuiWindowDockStyleCol_TabSelected,
    ImGuiWindowDockStyleCol_TabSelectedOverline,
    ImGuiWindowDockStyleCol_TabDimmed,
    ImGuiWindowDockStyleCol_TabDimmedSelected,
    ImGuiWindowDockStyleCol_TabDimmedSelectedOverline,
    ImGuiWindowDockStyleCol_COUNT
}ImGuiWindowDockStyleCol;
struct ImGuiWindowDockStyle
{
    ImU32 Colors[ImGuiWindowDockStyleCol_COUNT];
};
typedef struct ImGuiViewportP ImGuiViewportP;
struct ImGuiViewportP
{
    ImGuiViewport _ImGuiViewport;
    ImGuiWindow* Window;
    int Idx;
    int LastFrameActive;
    int LastFocusedStampCount;
    ImGuiID LastNameHash;
    ImVec2 LastPos;
    ImVec2 LastSize;
    float Alpha;
    float LastAlpha;
    _Bool LastFocusedHadNavWindow;
    short PlatformMonitor;
    int BgFgDrawListsLastFrame[2];
    ImDrawList* BgFgDrawLists[2];
    ImDrawData DrawDataP;
    ImDrawDataBuilder DrawDataBuilder;
    ImVec2 LastPlatformPos;
    ImVec2 LastPlatformSize;
    ImVec2 LastRendererSize;
    ImVec2 WorkInsetMin;
    ImVec2 WorkInsetMax;
    ImVec2 BuildWorkInsetMin;
    ImVec2 BuildWorkInsetMax;
};
typedef struct ImVector_unsigned_char {int Size;int Capacity;unsigned char* Data;} ImVector_unsigned_char;
typedef struct ImVector_ImGuiID {int Size;int Capacity;ImGuiID* Data;} ImVector_ImGuiID;
struct ImGuiWindowTempData
{
    ImVec2 CursorPos;
    ImVec2 CursorPosPrevLine;
    ImVec2 CursorStartPos;
    ImVec2 CursorMaxPos;
    ImVec2 IdealMaxPos;
    ImVec2 CurrLineSize;
    ImVec2 PrevLineSize;
    float CurrLineTextBaseOffset;
    float PrevLineTextBaseOffset;
    _Bool IsSameLine;
    _Bool IsSetPos;
    ImVec1 Indent;
    ImVec1 ColumnsOffset;
    ImVec1 GroupOffset;
    ImVec2 CursorStartPosLossyness;
    ImGuiNavLayer NavLayerCurrent;
    short NavLayersActiveMask;
    short NavLayersActiveMaskNext;
    _Bool NavIsScrollPushableX;
    _Bool NavHideHighlightOneFrame;
    _Bool NavWindowHasScrollY;
    _Bool MenuBarAppending;
    ImVec2 MenuBarOffset;
    ImGuiMenuColumns MenuColumns;
    int TreeDepth;
    ImU32 TreeHasStackDataDepthMask;
    ImU32 TreeRecordsClippedNodesY2Mask;
    ImVector_ImGuiWindowPtr ChildWindows;
    ImGuiStorage* StateStorage;
    ImGuiOldColumns* CurrentColumns;
    int CurrentTableIdx;
    ImGuiLayoutType LayoutType;
    ImGuiLayoutType ParentLayoutType;
    ImU32 ModalDimBgColor;
    ImGuiItemStatusFlags WindowItemStatusFlags;
    ImGuiItemStatusFlags ChildItemStatusFlags;
    ImGuiItemStatusFlags DockTabItemStatusFlags;
    ImRect DockTabItemRect;
    float ItemWidth;
    float TextWrapPos;
    ImVector_float ItemWidthStack;
    ImVector_float TextWrapPosStack;
};
typedef struct ImVector_ImGuiOldColumns {int Size;int Capacity;ImGuiOldColumns* Data;} ImVector_ImGuiOldColumns;
struct ImGuiWindow
{
    ImGuiContext* Ctx;
    char* Name;
    ImGuiID ID;
    ImGuiWindowFlags Flags, FlagsPreviousFrame;
    ImGuiChildFlags ChildFlags;
    ImGuiWindowClass WindowClass;
    ImGuiViewportP* Viewport;
    ImGuiID ViewportId;
    ImVec2 ViewportPos;
    int ViewportAllowPlatformMonitorExtend;
    ImVec2 Pos;
    ImVec2 Size;
    ImVec2 SizeFull;
    ImVec2 ContentSize;
    ImVec2 ContentSizeIdeal;
    ImVec2 ContentSizeExplicit;
    ImVec2 WindowPadding;
    float WindowRounding;
    float WindowBorderSize;
    float TitleBarHeight, MenuBarHeight;
    float DecoOuterSizeX1, DecoOuterSizeY1;
    float DecoOuterSizeX2, DecoOuterSizeY2;
    float DecoInnerSizeX1, DecoInnerSizeY1;
    int NameBufLen;
    ImGuiID MoveId;
    ImGuiID TabId;
    ImGuiID ChildId;
    ImGuiID PopupId;
    ImVec2 Scroll;
    ImVec2 ScrollMax;
    ImVec2 ScrollTarget;
    ImVec2 ScrollTargetCenterRatio;
    ImVec2 ScrollTargetEdgeSnapDist;
    ImVec2 ScrollbarSizes;
    _Bool ScrollbarX, ScrollbarY;
    _Bool ScrollbarXStabilizeEnabled;
    ImU8 ScrollbarXStabilizeToggledHistory;
    _Bool ViewportOwned;
    _Bool Active;
    _Bool WasActive;
    _Bool WriteAccessed;
    _Bool Collapsed;
    _Bool WantCollapseToggle;
    _Bool SkipItems;
    _Bool SkipRefresh;
    _Bool Appearing;
    _Bool Hidden;
    _Bool IsFallbackWindow;
    _Bool IsExplicitChild;
    _Bool HasCloseButton;
    signed char ResizeBorderHovered;
    signed char ResizeBorderHeld;
    short BeginCount;
    short BeginCountPreviousFrame;
    short BeginOrderWithinParent;
    short BeginOrderWithinContext;
    short FocusOrder;
    ImS8 AutoFitFramesX, AutoFitFramesY;
    _Bool AutoFitOnlyGrows;
    ImGuiDir AutoPosLastDirection;
    ImS8 HiddenFramesCanSkipItems;
    ImS8 HiddenFramesCannotSkipItems;
    ImS8 HiddenFramesForRenderOnly;
    ImS8 DisableInputsFrames;
    ImGuiCond SetWindowPosAllowFlags : 8;
    ImGuiCond SetWindowSizeAllowFlags : 8;
    ImGuiCond SetWindowCollapsedAllowFlags : 8;
    ImGuiCond SetWindowDockAllowFlags : 8;
    ImVec2 SetWindowPosVal;
    ImVec2 SetWindowPosPivot;
    ImVector_ImGuiID IDStack;
    ImGuiWindowTempData DC;
    ImRect OuterRectClipped;
    ImRect InnerRect;
    ImRect InnerClipRect;
    ImRect WorkRect;
    ImRect ParentWorkRect;
    ImRect ClipRect;
    ImRect ContentRegionRect;
    ImVec2ih HitTestHoleSize;
    ImVec2ih HitTestHoleOffset;
    int LastFrameActive;
    int LastFrameJustFocused;
    float LastTimeActive;
    float ItemWidthDefault;
    ImGuiStorage StateStorage;
    ImVector_ImGuiOldColumns ColumnsStorage;
    float FontWindowScale;
    float FontWindowScaleParents;
    float FontRefSize;
    int SettingsOffset;
    ImDrawList* DrawList;
    ImDrawList DrawListInst;
    ImGuiWindow* ParentWindow;
    ImGuiWindow* ParentWindowInBeginStack;
    ImGuiWindow* RootWindow;
    ImGuiWindow* RootWindowPopupTree;
    ImGuiWindow* RootWindowDockTree;
    ImGuiWindow* RootWindowForTitleBarHighlight;
    ImGuiWindow* RootWindowForNav;
    ImGuiWindow* ParentWindowForFocusRoute;
    ImGuiWindow* NavLastChildNavWindow;
    ImGuiID NavLastIds[ImGuiNavLayer_COUNT];
    ImRect NavRectRel[ImGuiNavLayer_COUNT];
    ImVec2 NavPreferredScoringPosRel[ImGuiNavLayer_COUNT];
    ImGuiID NavRootFocusScopeId;
    int MemoryDrawListIdxCapacity;
    int MemoryDrawListVtxCapacity;
    _Bool MemoryCompacted;
    _Bool DockIsActive :1;
    _Bool DockNodeIsVisible :1;
    _Bool DockTabIsVisible :1;
    _Bool DockTabWantClose :1;
    short DockOrder;
    ImGuiWindowDockStyle DockStyle;
    ImGuiDockNode* DockNode;
    ImGuiDockNode* DockNodeAsHost;
    ImGuiID DockId;
};
struct ImGuiTabItem
{
    ImGuiID ID;
    ImGuiTabItemFlags Flags;
    ImGuiWindow* Window;
    int LastFrameVisible;
    int LastFrameSelected;
    float Offset;
    float Width;
    float ContentWidth;
    float RequestedWidth;
    ImS32 NameOffset;
    ImS16 BeginOrder;
    ImS16 IndexDuringLayout;
    _Bool WantClose;
};
typedef struct ImVector_ImGuiTabItem {int Size;int Capacity;ImGuiTabItem* Data;} ImVector_ImGuiTabItem;
struct ImGuiTabBar
{
    ImGuiWindow* Window;
    ImVector_ImGuiTabItem Tabs;
    ImGuiTabBarFlags Flags;
    ImGuiID ID;
    ImGuiID SelectedTabId;
    ImGuiID NextSelectedTabId;
    ImGuiID VisibleTabId;
    int CurrFrameVisible;
    int PrevFrameVisible;
    ImRect BarRect;
    float BarRectPrevWidth;
    float CurrTabsContentsHeight;
    float PrevTabsContentsHeight;
    float WidthAllTabs;
    float WidthAllTabsIdeal;
    float ScrollingAnim;
    float ScrollingTarget;
    float ScrollingTargetDistToVisibility;
    float ScrollingSpeed;
    float ScrollingRectMinX;
    float ScrollingRectMaxX;
    float SeparatorMinX;
    float SeparatorMaxX;
    ImGuiID ReorderRequestTabId;
    ImS16 ReorderRequestOffset;
    ImS8 BeginCount;
    _Bool WantLayout;
    _Bool VisibleTabWasSubmitted;
    _Bool TabsAddedNew;
    _Bool ScrollButtonEnabled;
    ImS16 TabsActiveCount;
    ImS16 LastTabItemIdx;
    float ItemSpacingY;
    ImVec2 FramePadding;
    ImVec2 BackupCursorPos;
    ImGuiTextBuffer TabsNames;
};
struct ImFontLoader
{
    const char* Name;
    _Bool (*LoaderInit)(ImFontAtlas* atlas);
    void (*LoaderShutdown)(ImFontAtlas* atlas);
    _Bool (*FontSrcInit)(ImFontAtlas* atlas, ImFontConfig* src);
    void (*FontSrcDestroy)(ImFontAtlas* atlas, ImFontConfig* src);
    _Bool (*FontSrcContainsGlyph)(ImFontAtlas* atlas, ImFontConfig* src, ImWchar codepoint);
    _Bool (*FontBakedInit)(ImFontAtlas* atlas, ImFontConfig* src, ImFontBaked* baked, void* loader_data_for_baked_src);
    void (*FontBakedDestroy)(ImFontAtlas* atlas, ImFontConfig* src, ImFontBaked* baked, void* loader_data_for_baked_src);
    _Bool (*FontBakedLoadGlyph)(ImFontAtlas* atlas, ImFontConfig* src, ImFontBaked* baked, void* loader_data_for_baked_src, ImWchar codepoint, ImFontGlyph* out_glyph, float* out_advance_x);
    size_t FontBakedSrcLoaderDataSize;
};
struct ImFontAtlasRectEntry
{
    int TargetIndex : 20;
    unsigned int Generation : 10;
    unsigned int IsUsed : 1;
};
struct stbrp_node;
typedef stbrp_node stbrp_node_im;
typedef struct stbrp_context_opaque stbrp_context_opaque;
struct stbrp_context_opaque
{ char data[80];
};
typedef struct ImVector_stbrp_node_im {int Size;int Capacity;stbrp_node_im* Data;} ImVector_stbrp_node_im;
typedef struct ImVector_ImFontAtlasRectEntry {int Size;int Capacity;ImFontAtlasRectEntry* Data;} ImVector_ImFontAtlasRectEntry;
typedef struct ImVector_ImFontBakedPtr {int Size;int Capacity;ImFontBaked** Data;} ImVector_ImFontBakedPtr;
typedef struct ImStableVector_ImFontBaked__32 {int Size;int Capacity;ImVector_ImFontBakedPtr Blocks;} ImStableVector_ImFontBaked__32;
struct ImFontAtlasBuilder
{
    stbrp_context_opaque PackContext;
    ImVector_stbrp_node_im PackNodes;
    ImVector_ImTextureRect Rects;
    ImVector_ImFontAtlasRectEntry RectsIndex;
    ImVector_unsigned_char TempBuffer;
    int RectsIndexFreeListStart;
    int RectsPackedCount;
    int RectsPackedSurface;
    int RectsDiscardedCount;
    int RectsDiscardedSurface;
    int FrameCount;
    ImVec2i MaxRectSize;
    ImVec2i MaxRectBounds;
    _Bool LockDisableResize;
    _Bool PreloadedAllGlyphsRanges;
    ImStableVector_ImFontBaked__32 BakedPool;
    ImGuiStorage BakedMap;
    int BakedDiscardedCount;
    ImFontAtlasRectId PackIdMouseCursors;
    ImFontAtlasRectId PackIdLinesTexData;
};
extern  ImVec2* ImVec2_ImVec2_Nil(void);
extern  void ImVec2_destroy(ImVec2* self);
extern  ImVec2* ImVec2_ImVec2_Float(float _x,float _y);
extern  ImGuiContext* igCreateContext(ImFontAtlas* shared_font_atlas);
extern  void igDestroyContext(ImGuiContext* ctx);
extern  ImGuiContext* igGetCurrentContext(void);
extern  void igSetCurrentContext(ImGuiContext* ctx);
extern  ImGuiIO* igGetIO_Nil(void);
extern  ImGuiStyle* igGetStyle(void);
extern  void igNewFrame(void);
extern  void igEndFrame(void);
extern  void igRender(void);
extern  ImDrawData* igGetDrawData(void);
extern  const char* igGetVersion(void);
extern  _Bool igBegin(const char* name,_Bool* p_open,ImGuiWindowFlags flags);
extern  void igEnd(void);
extern  _Bool igBeginChild_Str(const char* str_id,const ImVec2 size,ImGuiChildFlags child_flags,ImGuiWindowFlags window_flags);
extern  void igEndChild(void);
extern  _Bool igIsWindowFocused(ImGuiFocusedFlags flags);
extern  _Bool igIsWindowHovered(ImGuiHoveredFlags flags);
extern  ImDrawList* igGetWindowDrawList(void);
extern  void igGetWindowPos(ImVec2 *pOut);
extern  void igGetWindowSize(ImVec2 *pOut);
extern  void igSetNextWindowPos(const ImVec2 pos,ImGuiCond cond,const ImVec2 pivot);
extern  void igSetNextWindowSize(const ImVec2 size,ImGuiCond cond);
extern  void igSetNextWindowSizeConstraints(const ImVec2 size_min,const ImVec2 size_max,ImGuiSizeCallback custom_callback,void* custom_callback_data);
extern  void igSetScrollHereY(float center_y_ratio);
extern  void igPushStyleColor_U32(ImGuiCol idx,ImU32 col);
extern  void igPopStyleColor(int count);
extern  void igSetNextItemWidth(float item_width);
extern  void igPushTextWrapPos(float wrap_local_pos_x);
extern  void igPopTextWrapPos(void);
extern  void igGetCursorScreenPos(ImVec2 *pOut);
extern  void igSetCursorScreenPos(const ImVec2 pos);
extern  void igGetContentRegionAvail(ImVec2 *pOut);
extern  void igSeparator(void);
extern  void igSameLine(float offset_from_start_x,float spacing);
extern  void igDummy(const ImVec2 size);
extern  void igIndent(float indent_w);
extern  void igUnindent(float indent_w);
extern  void igTextUnformatted(const char* text,const char* text_end);
extern  void igTextColored(const ImVec4 col,const char* fmt,...);
extern  _Bool igButton(const char* label,const ImVec2 size);
extern  _Bool igInvisibleButton(const char* str_id,const ImVec2 size,ImGuiButtonFlags flags);
extern  _Bool igCheckbox(const char* label,_Bool* v);
extern  void igImage(ImTextureRef tex_ref,const ImVec2 image_size,const ImVec2 uv0,const ImVec2 uv1);
extern  _Bool igImageButton(const char* str_id,ImTextureRef tex_ref,const ImVec2 image_size,const ImVec2 uv0,const ImVec2 uv1,const ImVec4 bg_col,const ImVec4 tint_col);
extern  _Bool igBeginCombo(const char* label,const char* preview_value,ImGuiComboFlags flags);
extern  void igEndCombo(void);
extern  _Bool igCombo_Str(const char* label,int* current_item,const char* items_separated_by_zeros,int popup_max_height_in_items);
extern  _Bool igSliderFloat(const char* label,float* v,float v_min,float v_max,const char* format,ImGuiSliderFlags flags);
extern  _Bool igSliderFloat2(const char* label,float v[2],float v_min,float v_max,const char* format,ImGuiSliderFlags flags);
extern  _Bool igSliderFloat4(const char* label,float v[4],float v_min,float v_max,const char* format,ImGuiSliderFlags flags);
extern  _Bool igSliderInt(const char* label,int* v,int v_min,int v_max,const char* format,ImGuiSliderFlags flags);
extern  _Bool igVSliderFloat(const char* label,const ImVec2 size,float* v,float v_min,float v_max,const char* format,ImGuiSliderFlags flags);
extern  _Bool igInputText(const char* label,char* buf,size_t buf_size,ImGuiInputTextFlags flags,ImGuiInputTextCallback callback,void* user_data);
extern  _Bool igColorEdit4(const char* label,float col[4],ImGuiColorEditFlags flags);
extern  _Bool igColorPicker4(const char* label,float col[4],ImGuiColorEditFlags flags,const float* ref_col);
extern  _Bool igTreeNode_Str(const char* label);
extern  void igTreePop(void);
extern  _Bool igCollapsingHeader_TreeNodeFlags(const char* label,ImGuiTreeNodeFlags flags);
extern  _Bool igSelectable_Bool(const char* label,_Bool selected,ImGuiSelectableFlags flags,const ImVec2 size);
extern  void igSetTooltip(const char* fmt,...);
extern  _Bool igBeginTable(const char* str_id,int columns,ImGuiTableFlags flags,const ImVec2 outer_size,float inner_width);
extern  void igEndTable(void);
extern  void igTableNextRow(ImGuiTableRowFlags row_flags,float min_row_height);
extern  _Bool igTableNextColumn(void);
extern  void igTableSetupColumn(const char* label,ImGuiTableColumnFlags flags,float init_width_or_weight,ImGuiID user_id);
extern  void igTableHeadersRow(void);
extern  ImGuiID igDockSpaceOverViewport(ImGuiID dockspace_id,const ImGuiViewport* viewport,ImGuiDockNodeFlags flags,const ImGuiWindowClass* window_class);
extern  void igSetNextWindowDockID(ImGuiID dock_id,ImGuiCond cond);
extern  void igBeginDisabled(_Bool disabled);
extern  void igEndDisabled(void);
extern  void igSetItemDefaultFocus(void);
extern  void igSetKeyboardFocusHere(int offset);
extern  _Bool igIsItemHovered(ImGuiHoveredFlags flags);
extern  _Bool igIsItemActive(void);
extern  _Bool igIsItemVisible(void);
extern  _Bool igIsItemActivated(void);
extern  _Bool igIsItemDeactivated(void);
extern  _Bool igIsItemDeactivatedAfterEdit(void);
extern  _Bool igIsAnyItemHovered(void);
extern  _Bool igIsAnyItemActive(void);
extern  void igGetItemRectMin(ImVec2 *pOut);
extern  void igGetItemRectMax(ImVec2 *pOut);
extern  ImGuiViewport* igGetMainViewport(void);
extern  ImDrawList* igGetForegroundDrawList_ViewportPtr(ImGuiViewport* viewport);
extern  _Bool igIsRectVisible_Nil(const ImVec2 size);
extern  _Bool igIsRectVisible_Vec2(const ImVec2 rect_min,const ImVec2 rect_max);
extern  void igCalcTextSize(ImVec2 *pOut,const char* text,const char* text_end,_Bool hide_text_after_double_hash,float wrap_width);
extern  _Bool igIsKeyDown_Nil(ImGuiKey key);
extern  _Bool igIsKeyPressed_Bool(ImGuiKey key,_Bool repeat);
extern  _Bool igIsMouseDown_Nil(ImGuiMouseButton button);
extern  void igGetMousePos(ImVec2 *pOut);
extern  void igGetMouseDragDelta(ImVec2 *pOut,ImGuiMouseButton button,float lock_threshold);
extern  void ImGuiStyle_ScaleAllSizes(ImGuiStyle* self,float scale_factor);
extern  void ImGuiIO_AddKeyEvent(ImGuiIO* self,ImGuiKey key,_Bool down);
extern  void ImGuiIO_AddMousePosEvent(ImGuiIO* self,float x,float y);
extern  void ImGuiIO_AddMouseButtonEvent(ImGuiIO* self,int button,_Bool down);
extern  void ImGuiIO_AddInputCharactersUTF8(ImGuiIO* self,const char* str);
extern  void ImDrawList_AddLine(ImDrawList* self,const ImVec2 p1,const ImVec2 p2,ImU32 col,float thickness);
extern  void ImDrawList_AddRectFilled(ImDrawList* self,const ImVec2 p_min,const ImVec2 p_max,ImU32 col,float rounding,ImDrawFlags flags);
extern  void ImDrawList_AddQuadFilled(ImDrawList* self,const ImVec2 p1,const ImVec2 p2,const ImVec2 p3,const ImVec2 p4,ImU32 col);
extern  void ImDrawList_AddCircleFilled(ImDrawList* self,const ImVec2 center,float radius,ImU32 col,int num_segments);
extern  void ImDrawList_AddImageQuad(ImDrawList* self,ImTextureRef tex_ref,const ImVec2 p1,const ImVec2 p2,const ImVec2 p3,const ImVec2 p4,const ImVec2 uv1,const ImVec2 uv2,const ImVec2 uv3,const ImVec2 uv4,ImU32 col);
extern  _Bool ImGuiDockNode_IsEmpty(ImGuiDockNode* self);
extern  ImGuiWindow* igGetCurrentWindowRead(void);
extern  void igClearIniSettings(void);
extern  void igClearActiveID(void);
extern  void igDockBuilderDockWindow(const char* window_name,ImGuiID node_id);
extern  ImGuiDockNode* igDockBuilderGetNode(ImGuiID node_id);
extern  ImGuiDockNode* igDockBuilderGetCentralNode(ImGuiID node_id);
extern  ImGuiID igDockBuilderAddNode(ImGuiID node_id,ImGuiDockNodeFlags flags);
extern  void igDockBuilderRemoveNode(ImGuiID node_id);
extern  void igDockBuilderSetNodeSize(ImGuiID node_id,ImVec2 size);
extern  ImGuiID igDockBuilderSplitNode(ImGuiID node_id,ImGuiDir split_dir,float size_ratio_for_node_at_dir,ImGuiID* out_id_at_dir,ImGuiID* out_id_at_opposite_dir);
extern  void igDockBuilderFinish(ImGuiID node_id);
extern  float igGET_FLT_MAX(void);
extern  float igGET_FLT_MIN(void);

]]