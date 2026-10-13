cmake_minimum_required(VERSION 3.21)

project(codeisland-shell VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

find_package(Qt6 REQUIRED COMPONENTS Core Gui Qml Quick Network)

# Where the QML lives at run time. Development reads straight from the source
# tree so editing a .qml file needs no rebuild; packaging overrides this.
set(CODEISLAND_QML_DIR "${CMAKE_CURRENT_SOURCE_DIR}/qml" CACHE STRING
    "Directory containing the shell's QML files")

qt_add_executable(codeisland-shell
    src/main.cpp
    src/daemon_client.cpp
    src/sessions_model.cpp
)

target_compile_definitions(codeisland-shell PRIVATE
    CODEISLAND_QML_DIR="${CODEISLAND_QML_DIR}"
)

target_link_libraries(codeisland-shell PRIVATE
    Qt6::Core
    Qt6::Gui
    Qt6::Qml
    Qt6::Quick
    Qt6::Network
)
