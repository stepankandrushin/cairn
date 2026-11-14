// The app driven as a person would, against a server the test run names: sign in, back up the photo
// library, and find the photos on the server by browsing it.
//
// TEST_RUNNER_CAIRN_SERVER and TEST_RUNNER_CAIRN_TOKEN, given to xcodebuild, name the server and its
// token; scripts/release-ios.sh --sim gives them, with the library's photos access granted first.

import XCTest

final class CairnUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    func test1SignInBackUpAndBrowse() throws {
        let env = ProcessInfo.processInfo.environment
        guard let server = env["CAIRN_SERVER"], let token = env["CAIRN_TOKEN"] else {
            throw XCTSkip("CAIRN_SERVER and CAIRN_TOKEN name the server to test against")
        }
        let folder = env["CAIRN_UPLOAD_PATH"] ?? "phone/Simulator"
        let app = XCUIApplication()
        app.launch()

        let field = app.textFields["server"]
        if field.waitForExistence(timeout: 10) {
            field.tap()
            field.press(forDuration: 1)
            if app.menuItems["Select All"].waitForExistence(timeout: 2) { app.menuItems["Select All"].tap() }
            field.typeText(server)
            app.secureTextFields["token"].tap()
            app.secureTextFields["token"].typeText(token)
            let name = app.textFields["deviceName"]
            name.tap()
            name.press(forDuration: 1)
            if app.menuItems["Select All"].waitForExistence(timeout: 2) { app.menuItems["Select All"].tap() }
            name.typeText("Simulator")
            shot(app, "1 sign in")
            app.buttons["signIn"].tap()
        }

        XCTAssertTrue(app.tabBars.buttons["Photos"].waitForExistence(timeout: 20), "not signed in")
        app.tabBars.buttons["Photos"].tap()
        let start = app.buttons["startBackup"]
        if start.waitForExistence(timeout: 5) {
            let path = app.textFields["backupPath"]
            path.tap()
            path.press(forDuration: 1)
            if app.menuItems["Select All"].waitForExistence(timeout: 2) { app.menuItems["Select All"].tap() }
            path.typeText(folder)
            app.buttons["The whole library"].firstMatch.tap()
            shot(app, "2 backup settings")
            start.tap()
        }
        let waiting = app.staticTexts["waitingCount"]
        XCTAssertTrue(app.buttons["uploadNow"].waitForExistence(timeout: 20), "backup did not start")
        // Sent when nothing waits and something was sent.
        let deadline = Date().addingTimeInterval(180)
        while Date() < deadline {
            let sent = app.staticTexts["sentCount"]
            if sent.exists, sent.label.hasSuffix(" 0") == false, waiting.exists, waiting.label.hasSuffix(" 0") {
                break
            }
            sleep(2)
        }
        shot(app, "3 backed up")
        XCTAssertFalse(app.staticTexts["sentCount"].label.hasSuffix(" 0"), "nothing was sent")

        app.tabBars.buttons["Browse"].tap()
        app.cells.firstMatch.tap()
        for part in folder.split(separator: "/") {
            let link = app.buttons[String(part)].firstMatch
            XCTAssertTrue(link.waitForExistence(timeout: 20), "\(part) is not on the server")
            link.tap()
        }
        let photo = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'entry-'")).firstMatch
        XCTAssertTrue(photo.waitForExistence(timeout: 20), "no photo in \(folder)")
        sleep(3)
        shot(app, "4 browsed")
        photo.tap()
        XCTAssertTrue(app.scrollViews["photo"].waitForExistence(timeout: 30), "the photo did not open")
        shot(app, "5 opened")
    }

    /// The Files app, after the first test signed in: the folder is a location there, its folders listed
    /// when opened, and a file downloaded when it is opened.
    func test2FilesApp() throws {
        let env = ProcessInfo.processInfo.environment
        guard env["CAIRN_SERVER"] != nil else { throw XCTSkip("CAIRN_SERVER names the server to test against") }
        let folder = env["CAIRN_UPLOAD_PATH"] ?? "phone/Simulator"
        let files = XCUIApplication(bundleIdentifier: "com.apple.DocumentsApp")
        files.launch()
        let browse = files.tabBars.buttons["Browse"]
        if browse.waitForExistence(timeout: 10) {
            browse.tap()
            browse.tap()
        }
        let location = files.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS 'Cairn'")).firstMatch
        shot(files, "6 files locations")
        XCTAssertTrue(location.waitForExistence(timeout: 30), "no Cairn location in Files")
        location.tap()
        for part in folder.split(separator: "/") {
            let item = files.descendants(matching: .any).matching(NSPredicate(format: "label BEGINSWITH %@", String(part))).firstMatch
            XCTAssertTrue(item.waitForExistence(timeout: 30), "\(part) is not listed in Files")
            item.tap()
        }
        sleep(3)
        shot(files, "7 files folder")
        // Files may hide the extension: a photo of the simulator's library, by its number.
        let photo = files.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS ' 0002'")).firstMatch
        XCTAssertTrue(photo.waitForExistence(timeout: 30), "no photo listed in Files")
        photo.tap()
        sleep(8)
        shot(files, "8 files opened")
    }

    private func shot(_ app: XCUIApplication, _ name: String) {
        let a = XCTAttachment(screenshot: app.screenshot())
        a.name = name
        a.lifetime = .keepAlways
        add(a)
    }
}
